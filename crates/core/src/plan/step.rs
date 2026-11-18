use super::need::Need;

/// One question, `request`, for the judge named `judge`, asked once the
/// steps it waits on allow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step<Q> {
    id: String,
    judge: String,
    request: Q,
    all: Vec<Need>,
    any: Vec<Need>,
}

impl<Q> Step<Q> {
    /// A step with its ID, the name of the judge that answers it, and its request.
    pub fn new(id: impl Into<String>, judge: impl Into<String>, request: Q) -> Self {
        Self {
            id: id.into(),
            judge: judge.into(),
            request,
            all: Vec::new(),
            any: Vec::new(),
        }
    }

    /// The step, waiting until every one of `needs` holds.
    #[must_use]
    pub fn after(mut self, needs: impl IntoIterator<Item = Need>) -> Self {
        self.all.extend(needs);
        self
    }

    /// The step, waiting until at least one of `needs` holds. With
    /// [`Step::after`] too, it runs only when both hold.
    #[must_use]
    pub fn after_any(mut self, needs: impl IntoIterator<Item = Need>) -> Self {
        self.any.extend(needs);
        self
    }

    /// The step's ID, unique within its plan.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The name of the judge that answers it.
    #[must_use]
    pub fn judge(&self) -> &str {
        &self.judge
    }

    /// What the judge is asked.
    #[must_use]
    pub const fn request(&self) -> &Q {
        &self.request
    }

    /// The needs that must all hold.
    #[must_use]
    pub fn all(&self) -> &[Need] {
        &self.all
    }

    /// The needs of which at least one must hold.
    #[must_use]
    pub fn any(&self) -> &[Need] {
        &self.any
    }
}
