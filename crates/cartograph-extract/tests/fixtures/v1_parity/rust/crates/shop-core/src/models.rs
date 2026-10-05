use std::fmt;
use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

pub trait Describe {
    fn describe(&self) -> String;
}

pub trait Priced: Describe + Clone + Send + for<'a> Fn(&'a u8) + Iterator<Item = u8> {
    fn price(&self) -> u32;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineItem {
    pub sku: String,
    pub qty: u32,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub id: crate::OrderId,
    pub items: Vec<LineItem>,
    tags: HashSet<String>,
    meta: HashMap<String, String>,
    pub status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Status {
    Pending,
    Paid { amount: u32 },
    Shipped(u64),
}

pub struct Tuple(pub u32, pub String);

pub union Bits {
    pub raw: u32,
    pub half: u16,
}

impl Order {
    pub fn new(id: crate::OrderId) -> Self {
        Order { id, items: Vec::new(), tags: HashSet::new(), meta: HashMap::new(), status: Status::Pending }
    }

    pub fn add(&mut self, item: LineItem) -> &mut Self {
        self.items.push(item);
        self
    }

    pub fn total(&self) -> u32 {
        self.items.iter().map(|i| i.qty).sum()
    }

    pub async fn sync(&self) -> bool {
        self.total() > 0
    }
}

impl Describe for Order {
    fn describe(&self) -> String {
        format!("order {}", self.id)
    }
}

impl fmt::Display for Order {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.describe())
    }
}

impl<T: Describe> Describe for Vec<T> {
    fn describe(&self) -> String {
        self.iter().map(Describe::describe).collect::<Vec<_>>().join(",")
    }
}
