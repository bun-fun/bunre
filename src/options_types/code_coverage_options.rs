//! Reporter selection for the (now removed) test runner. `TestOptions` still
//! carries a `Reporters`, so only the option struct that fed the deleted
//! `--coverage` CLI flags is gone.#[derive(Clone, Copy)]
pub struct Reporters {
    pub text: bool,
    pub lcov: bool,
}
