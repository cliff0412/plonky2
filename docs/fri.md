```rust
#[derive(Debug)]
pub struct FriChallenges<F: RichField + Extendable<D>, const D: usize> {
    // Scaling factor to combine polynomials.
    pub fri_alpha: F::Extension,

    // Betas used in the FRI commit phase reductions.
    pub fri_betas: Vec<F::Extension>,

    pub fri_pow_response: F,

    // Indices at which the oracle is queried in FRI.
    pub fri_query_indices: Vec<usize>,
}
```