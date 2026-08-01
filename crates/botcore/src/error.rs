/// How the engine should react to a failed exchange interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient. Retry with backoff; safe because every order carries a
    /// deterministic `orderLinkId`.
    Retryable,
    /// The exchange understood and refused. Log, skip the signal, continue.
    Rejected,
    /// Unrecoverable without a human. Halt trading and alert.
    Fatal,
}
