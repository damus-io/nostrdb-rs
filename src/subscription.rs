
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Subscription(u64);

impl Subscription {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn id(self) -> u64 {
        self.0
    }
}
