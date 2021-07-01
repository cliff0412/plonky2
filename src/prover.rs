use std::time::Instant;

use itertools::Itertools;
use log::info;
use rayon::prelude::*;

use crate::circuit_data::{CommonCircuitData, ProverOnlyCircuitData};
use crate::field::extension_field::Extendable;
use crate::generator::generate_partial_witness;
use crate::plonk_challenger::Challenger;
use crate::plonk_common::{eval_vanishing_poly_base, PlonkPolynomials, ZeroPolyOnCoset};
use crate::polynomial::commitment::ListPolynomialCommitment;
use crate::polynomial::polynomial::{PolynomialCoeffs, PolynomialValues};
use crate::proof::Proof;
use crate::timed;
use crate::util::partial_products::partial_products;
use crate::util::{ceil_div_usize, log2_ceil, transpose};
use crate::vars::EvaluationVarsBase;
use crate::witness::{PartialWitness, Witness};

pub(crate) fn prove<F: Extendable<D>, const D: usize>(
    prover_data: &ProverOnlyCircuitData<F, D>,
    common_data: &CommonCircuitData<F, D>,
    inputs: PartialWitness<F>,
) -> Proof<F, D> {
    let fri_config = &common_data.config.fri_config;
    let config = &common_data.config;
    let num_wires = config.num_wires;
    let num_challenges = config.num_challenges;
    let quotient_degree = common_data.quotient_degree();
    let degree = common_data.degree();

    let start_proof_gen = Instant::now();

    let mut partial_witness = inputs;
    info!("Running {} generators", prover_data.generators.len());
    timed!(
        generate_partial_witness(&mut partial_witness, &prover_data.generators),
        "to generate witness"
    );

    let witness = timed!(
        partial_witness.full_witness(degree, num_wires),
        "to compute full witness"
    );

    timed!(
        witness
            .check_copy_constraints(&prover_data.copy_constraints, &prover_data.gate_instances)
            .unwrap(), // TODO: Change return value to `Result` and use `?` here.
        "to check copy constraints"
    );

    let wires_values: Vec<PolynomialValues<F>> = timed!(
        witness
            .wire_values
            .iter()
            .map(|column| PolynomialValues::new(column.clone()))
            .collect(),
        "to compute wire polynomials"
    );

    // TODO: Could try parallelizing the transpose, or not doing it explicitly, instead having
    // merkle_root_bit_rev_order do it implicitly.
    let wires_commitment = timed!(
        ListPolynomialCommitment::new(
            wires_values,
            fri_config.rate_bits,
            PlonkPolynomials::WIRES.blinding
        ),
        "to compute wires commitment"
    );

    let mut challenger = Challenger::new();
    // Observe the instance.
    // TODO: Need to include public inputs as well.
    challenger.observe_hash(&common_data.circuit_digest);

    challenger.observe_hash(&wires_commitment.merkle_tree.root);
    let betas = challenger.get_n_challenges(num_challenges);
    let gammas = challenger.get_n_challenges(num_challenges);

    let mut partial_products = timed!(
        all_wires_permutation_partial_products(&witness, &betas, &gammas, prover_data, common_data),
        "to compute partial products"
    );

    let plonk_z_vecs = timed!(
        compute_zs(&partial_products, prover_data, common_data),
        "to compute Z's"
    );

    partial_products.iter_mut().for_each(|part| {
        part.drain(0..2);
    });

    let zs_partial_products = [plonk_z_vecs, partial_products.concat()].concat();
    let plonk_zs_commitment = timed!(
        ListPolynomialCommitment::new(
            zs_partial_products,
            fri_config.rate_bits,
            PlonkPolynomials::ZS.blinding
        ),
        "to commit to Z's"
    );

    challenger.observe_hash(&plonk_zs_commitment.merkle_tree.root);

    let alphas = challenger.get_n_challenges(num_challenges);

    let quotient_polys = timed!(
        compute_quotient_polys(
            common_data,
            prover_data,
            &wires_commitment,
            &plonk_zs_commitment,
            &betas,
            &gammas,
            &alphas,
        ),
        "to compute vanishing polys"
    );

    // Compute the quotient polynomials, aka `t` in the Plonk paper.
    let all_quotient_poly_chunks = timed!(
        quotient_polys
            .into_par_iter()
            .flat_map(|mut quotient_poly| {
                quotient_poly.trim();
                quotient_poly.pad(quotient_degree).expect(
                    "The quotient polynomial doesn't have the right degree. \
                     This may be because the `Z`s polynomials are still too high degree.",
                );
                // Split t into degree-n chunks.
                quotient_poly.chunks(degree)
            })
            .collect(),
        "to compute quotient polys"
    );

    let quotient_polys_commitment = timed!(
        ListPolynomialCommitment::new_from_polys(
            all_quotient_poly_chunks,
            fri_config.rate_bits,
            PlonkPolynomials::QUOTIENT.blinding
        ),
        "to commit to quotient polys"
    );

    challenger.observe_hash(&quotient_polys_commitment.merkle_tree.root);

    let zeta = challenger.get_extension_challenge();

    let (opening_proof, openings) = timed!(
        ListPolynomialCommitment::open_plonk(
            &[
                &prover_data.constants_sigmas_commitment,
                &wires_commitment,
                &plonk_zs_commitment,
                &quotient_polys_commitment,
            ],
            zeta,
            &mut challenger,
            common_data,
        ),
        "to compute opening proofs"
    );

    info!(
        "{:.3}s for overall witness & proof generation",
        start_proof_gen.elapsed().as_secs_f32()
    );

    Proof {
        wires_root: wires_commitment.merkle_tree.root,
        plonk_zs_root: plonk_zs_commitment.merkle_tree.root,
        quotient_polys_root: quotient_polys_commitment.merkle_tree.root,
        openings,
        opening_proof,
    }
}

fn all_wires_permutation_partial_products<F: Extendable<D>, const D: usize>(
    witness: &Witness<F>,
    betas: &[F],
    gammas: &[F],
    prover_data: &ProverOnlyCircuitData<F, D>,
    common_data: &CommonCircuitData<F, D>,
) -> Vec<Vec<PolynomialValues<F>>> {
    (0..common_data.config.num_challenges)
        .map(|i| {
            wires_permutation_partial_products(
                witness,
                betas[i],
                gammas[i],
                prover_data,
                common_data,
            )
        })
        .collect()
}

fn wires_permutation_partial_products<F: Extendable<D>, const D: usize>(
    witness: &Witness<F>,
    beta: F,
    gamma: F,
    prover_data: &ProverOnlyCircuitData<F, D>,
    common_data: &CommonCircuitData<F, D>,
) -> Vec<PolynomialValues<F>> {
    let degree = common_data.max_filtered_constraint_degree;
    let subgroup = &prover_data.subgroup;
    let k_is = &common_data.k_is;
    let values = subgroup
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let s_sigmas = &prover_data.sigmas[i];
            let numerator_values = (0..common_data.config.num_routed_wires)
                .map(|j| {
                    let wire_value = witness.get_wire(i, j);
                    let k_i = k_is[j];
                    let s_id = k_i * x;
                    wire_value + beta * s_id + gamma
                })
                .collect::<Vec<_>>();
            let denominator_values = (0..common_data.config.num_routed_wires)
                .map(|j| {
                    let wire_value = witness.get_wire(i, j);
                    let s_sigma = s_sigmas[j];
                    wire_value + beta * s_sigma + gamma
                })
                .collect::<Vec<_>>();
            let numerator_partials = partial_products(&numerator_values, degree);
            let denominator_partials = partial_products(&denominator_values, degree);
            let numerator = numerator_partials.0
                [numerator_partials.0.len() - numerator_partials.1..]
                .iter()
                .copied()
                .product();
            let denominator = denominator_partials.0
                [denominator_partials.0.len() - denominator_partials.1..]
                .iter()
                .copied()
                .product();

            [
                vec![numerator],
                vec![denominator],
                numerator_partials.0,
                denominator_partials.0,
            ]
            .concat()
        })
        .collect::<Vec<_>>();

    transpose(&values)
        .into_par_iter()
        .map(PolynomialValues::new)
        .collect()
}

fn compute_zs<F: Extendable<D>, const D: usize>(
    partial_products: &[Vec<PolynomialValues<F>>],
    prover_data: &ProverOnlyCircuitData<F, D>,
    common_data: &CommonCircuitData<F, D>,
) -> Vec<PolynomialValues<F>> {
    (0..common_data.config.num_challenges)
        .map(|i| compute_z(&partial_products[i], prover_data, common_data))
        .collect()
}

fn compute_z<F: Extendable<D>, const D: usize>(
    partial_products: &[PolynomialValues<F>],
    prover_data: &ProverOnlyCircuitData<F, D>,
    common_data: &CommonCircuitData<F, D>,
) -> PolynomialValues<F> {
    let subgroup = &prover_data.subgroup;
    let mut plonk_z_points = vec![F::ONE];
    for i in 1..common_data.degree() {
        let x = subgroup[i - 1];
        let numerator = partial_products[0].values[i - 1];
        let denominator = partial_products[1].values[i - 1];
        let last = *plonk_z_points.last().unwrap();
        plonk_z_points.push(last * numerator / denominator);
    }
    plonk_z_points.into()
}

fn compute_quotient_polys<'a, F: Extendable<D>, const D: usize>(
    common_data: &CommonCircuitData<F, D>,
    prover_data: &'a ProverOnlyCircuitData<F, D>,
    wires_commitment: &'a ListPolynomialCommitment<F>,
    zs_partial_products_commitment: &'a ListPolynomialCommitment<F>,
    betas: &[F],
    gammas: &[F],
    alphas: &[F],
) -> Vec<PolynomialCoeffs<F>> {
    let num_challenges = common_data.config.num_challenges;
    let max_filtered_constraint_degree_bits = log2_ceil(common_data.max_filtered_constraint_degree);
    assert!(
        max_filtered_constraint_degree_bits <= common_data.config.rate_bits,
        "Having constraints of degree higher than the rate is not supported yet. \
        If we need this in the future, we can precompute the larger LDE before computing the `ListPolynomialCommitment`s."
    );

    // We reuse the LDE computed in `ListPolynomialCommitment` and extract every `step` points to get
    // an LDE matching `max_filtered_constraint_degree`.
    let step = 1 << (common_data.config.rate_bits - max_filtered_constraint_degree_bits);
    // When opening the `Z`s polys at the "next" point in Plonk, need to look at the point `next_step`
    // steps away since we work on an LDE of degree `max_filtered_constraint_degree`.
    let next_step = 1 << max_filtered_constraint_degree_bits;

    let points =
        F::two_adic_subgroup(common_data.degree_bits + max_filtered_constraint_degree_bits);
    let lde_size = points.len();

    // Retrieve the LDE values at index `i`.
    let get_at_index = |comm: &'a ListPolynomialCommitment<F>, i: usize| -> &'a [F] {
        comm.get_lde_values(i * step)
    };

    let z_h_on_coset =
        ZeroPolyOnCoset::new(common_data.degree_bits, max_filtered_constraint_degree_bits);

    let quotient_values: Vec<Vec<F>> = points
        // .into_par_iter()
        .into_iter()
        .enumerate()
        .map(|(i, x)| {
            let shifted_x = F::coset_shift() * x;
            let i_next = (i + next_step) % lde_size;
            let local_constants_sigmas = get_at_index(&prover_data.constants_sigmas_commitment, i);
            let local_constants = &local_constants_sigmas[common_data.constants_range()];
            let s_sigmas = &local_constants_sigmas[common_data.sigmas_range()];
            let local_wires = get_at_index(wires_commitment, i);
            let local_zs_partial_products = get_at_index(zs_partial_products_commitment, i);
            let local_zs = &local_zs_partial_products[common_data.zs_range()];
            let next_zs =
                &get_at_index(zs_partial_products_commitment, i_next)[common_data.zs_range()];
            let local_partial_products =
                &local_zs_partial_products[common_data.partial_products_range()];

            debug_assert_eq!(local_wires.len(), common_data.config.num_wires);
            debug_assert_eq!(local_zs.len(), num_challenges);

            let vars = EvaluationVarsBase {
                local_constants,
                local_wires,
            };
            dbg!(i);
            let mut quotient_values = eval_vanishing_poly_base(
                common_data,
                i,
                shifted_x,
                vars,
                local_zs,
                next_zs,
                local_partial_products,
                s_sigmas,
                betas,
                gammas,
                alphas,
                &z_h_on_coset,
            );
            let denominator_inv = z_h_on_coset.eval_inverse(i);
            quotient_values
                .iter_mut()
                .for_each(|v| *v *= denominator_inv);
            quotient_values
        })
        .collect();

    transpose(&quotient_values)
        .into_par_iter()
        .map(PolynomialValues::new)
        .map(|values| values.coset_ifft(F::coset_shift()))
        .collect()
}
