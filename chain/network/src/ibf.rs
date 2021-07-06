use std::cmp::{max, min};
use std::hash::Hasher;

use borsh::{BorshDeserialize, BorshSerialize};
use std::collections::hash_map::DefaultHasher;
use tracing::error;

const NUM_HASHES: usize = 3;

#[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Clone, Debug, Default)]
pub struct IbfElem {
    xor_elem: u64,
    xor_hash: u64,
}

impl IbfElem {
    fn adjust(&mut self, elem: u64, elem_hash: u64) {
        self.xor_elem ^= elem;
        self.xor_hash ^= elem_hash;
    }

    fn merge(&mut self, rhs: &IbfElem) {
        self.xor_elem ^= rhs.xor_elem;
        self.xor_hash ^= rhs.xor_hash;
    }
}

type IbfHasher = DefaultHasher;

#[derive(Clone)]
pub struct Ibf {
    k: i32,
    pub data: Vec<IbfElem>,
    hasher: IbfHasher,
    pub seed: u64,
}

impl Ibf {
    pub fn from_vec(data: Vec<IbfElem>, seed: u64) -> Self {
        let k = Ibf::calculate_k(data.len());

        let mut hasher = IbfHasher::default();
        hasher.write_u64(seed);
        Self { data, hasher, k, seed }
    }

    pub fn new(capacity: usize, seed: u64) -> Self {
        let k = Ibf::calculate_k(capacity);
        let new_capacity = (1 << k) + NUM_HASHES - 1;

        let mut hasher = IbfHasher::default();
        hasher.write_u64(seed);
        Self { data: vec![IbfElem::default(); new_capacity], hasher, k, seed }
    }

    fn calculate_k(capacity: usize) -> i32 {
        let mut k = 0;
        // In order make computation of indexes not require doing division, we allocate
        // 2^k + NUM_HASHES - 1 hashes. This allows us to use compute indexes using bits.
        while (1 << k) + NUM_HASHES - 1 < capacity {
            k += 1;
        }
        k
    }

    pub fn add(&mut self, elem: u64) {
        self.adjust(elem)
    }

    pub fn remove(&mut self, elem: u64) {
        self.adjust(elem)
    }

    fn compute_hash(&self, elem: u64) -> u64 {
        let mut h = self.hasher.clone();
        h.write_u64(elem);
        h.finish()
    }

    fn adjust(&mut self, elem: u64) {
        self.adjust_value(elem);
    }

    pub fn merge(&mut self, rhs_data: &[IbfElem], rhs_seed: u64) -> bool {
        if self.data.len() != rhs_data.len() || self.seed != rhs_seed {
            error!(
                "failed to merge len: {} {} seed: {} {}",
                self.data.len(),
                rhs_data.len(),
                self.seed,
                rhs_seed
            );
            return false;
        }
        for i in 0..self.data.len() {
            self.data[i].merge(&rhs_data[i]);
        }
        true
    }

    #[cfg(test)]
    fn recover(&mut self) -> Result<Vec<u64>, &'static str> {
        let (result, difference) = self.try_recover();

        if difference != 0 {
            for i in 0..self.data.len() {
                if self.data[i].xor_elem != 0 {
                    println!(
                        "{} {:?} {}",
                        i,
                        self.data[i],
                        self.compute_hash(self.data[i].xor_elem)
                    );
                }
            }
            return Err("unable to recover result");
        }
        return Ok(result);
    }

    pub fn try_recover(&mut self) -> (Vec<u64>, u64) {
        let mut result = Vec::with_capacity(self.data.len());
        let mut to_check = Vec::default();
        for i in 0..self.data.len() {
            to_check.push(i);

            while let Some(i) = to_check.pop() {
                let elem = self.data[i].xor_elem;
                if elem == 0 && self.data[i].xor_hash == 0 {
                    continue;
                }
                let elem_hash = self.compute_hash(elem);
                if elem_hash != self.data[i].xor_hash {
                    continue;
                }

                result.push(elem);
                self.adjust_value_and_add_to_queue(elem, elem_hash, &mut to_check);
            }
        }
        let mut elems_that_differ = 0;
        for i in 0..self.data.len() {
            if self.data[i].xor_elem != 0 {
                elems_that_differ += 1;
            }
        }
        (result, elems_that_differ)
    }

    fn generate_idx(&mut self, elem_hash: u64) -> [usize; NUM_HASHES] {
        let mask = (1 << self.k) - 1;
        let pos0 = elem_hash & mask;
        let mut pos1 = (elem_hash >> self.k) & mask;
        let mut pos2 = (elem_hash >> 2 * self.k) & mask;
        if pos1 >= pos0 {
            pos1 = (pos1 + 1) & mask;
        }
        if pos2 >= min(pos0, pos1) {
            pos2 = (pos2 + 1) & mask;
        }
        if pos2 >= max(pos0, pos1) {
            pos2 = (pos2 + 1) & mask;
        }
        [pos0 as usize, pos1 as usize, pos2 as usize]
    }

    fn adjust_value_and_add_to_queue(&mut self, elem: u64, elem_hash: u64, queue: &mut Vec<usize>) {
        let pos_list = self.generate_idx(elem_hash);

        for &pos in &pos_list {
            self.data[pos].adjust(elem, elem_hash);
            queue.push(pos);
        }
    }

    fn adjust_value(&mut self, elem: u64) {
        let elem_hash = self.compute_hash(elem);
        let pos_list = self.generate_idx(elem_hash);

        for &pos in &pos_list {
            self.data[pos].adjust(elem, elem_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ibf::Ibf;

    fn create_blt(elements: impl IntoIterator<Item = u64>, capacity: usize) -> Ibf {
        let mut sketch = Ibf::new(capacity, 0);
        for item in elements.into_iter() {
            sketch.add(item);
        }
        sketch
    }

    #[test]
    fn create_blt_test() {
        let set = 1000000_3_00000u64..1000000_301000u64;

        assert_eq!(1000, create_blt(set, 2048).recover().unwrap().len())
    }
}