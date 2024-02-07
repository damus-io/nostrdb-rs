use crate::Filter;

pub struct Subscription {
    pub filters: Vec<Filter>,
    pub id: u64,
}
