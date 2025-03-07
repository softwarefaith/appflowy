use crate::core::{Attributes, Delta, Operation};

pub struct DeltaBuilder<T: Attributes> {
    delta: Delta<T>,
}

impl<T> std::default::Default for DeltaBuilder<T>
where
    T: Attributes,
{
    fn default() -> Self { Self { delta: Delta::new() } }
}

impl<T> DeltaBuilder<T>
where
    T: Attributes,
{
    pub fn new() -> Self { DeltaBuilder::default() }

    pub fn retain_with_attributes(mut self, n: usize, attrs: T) -> Self {
        self.delta.retain(n, attrs);
        self
    }

    pub fn retain(mut self, n: usize) -> Self {
        self.delta.retain(n, T::default());
        self
    }

    pub fn delete(mut self, n: usize) -> Self {
        self.delta.delete(n);
        self
    }

    pub fn insert_with_attributes(mut self, s: &str, attrs: T) -> Self {
        self.delta.insert(s, attrs);
        self
    }

    pub fn insert(mut self, s: &str) -> Self {
        self.delta.insert(s, T::default());
        self
    }

    pub fn trim(mut self) -> Self {
        trim(&mut self.delta);
        self
    }

    pub fn build(self) -> Delta<T> { self.delta }
}

pub fn trim<T: Attributes>(delta: &mut Delta<T>) {
    let remove_last = match delta.ops.last() {
        None => false,
        Some(op) => match op {
            Operation::Delete(_) => false,
            Operation::Retain(retain) => retain.is_plain(),
            Operation::Insert(_) => false,
        },
    };
    if remove_last {
        delta.ops.pop();
    }
}
