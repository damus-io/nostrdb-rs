use crate::Filter;

#[derive(Debug, Clone, Copy)]
pub struct Subscription {
    pub filters: Vec<Filter>,
    pub id: u64,
}
