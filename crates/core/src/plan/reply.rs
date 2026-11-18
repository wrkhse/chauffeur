/// What core reads of a step's request: how many values its answer can settle to.
pub trait Request {
    /// The number of values, from 1; an answer settles to one of `0..outcomes()`.
    fn outcomes(&self) -> u8;
}

/// What core reads of an answer, to decide the needs that wait on its step.
pub trait Reply {
    /// The index of the value it settled to, below its request's `outcomes()`.
    fn settled(&self) -> u8;

    /// Its probability for outcome `outcome`, in thousandths (0 to 1000);
    /// none for no such outcome.
    fn p(&self, outcome: u8) -> Option<u16>;
}
