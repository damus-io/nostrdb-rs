use crate::Filter;

#[derive(Debug, Clone)]
pub struct Subscription {
    pub filters: Vec<Filter>,
    pub id: u64,
}
