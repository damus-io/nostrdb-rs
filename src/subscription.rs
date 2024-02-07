use crate::Filter;

pub struct Subscription<'a> {
    pub filter: &'a Filter,
    pub id: u64,
}
