//! `list.sort(key=...)`.
//!
//! Python's sort is stable, so with keys that are totally ordered any stable sort gives the same
//! result, and [`sort_by_f64_key`] uses Rust's. Keys that are not totally ordered (NaN) make the
//! result depend on Timsort's exact comparisons; those inputs go through [`timsort`], a port of
//! CPython's `listsort` that compares with `<` only.

use std::cmp::Ordering;

/// `items.sort(key=key)` for float keys.
pub fn sort_by_f64_key<T>(items: &mut Vec<T>, key: impl Fn(&T) -> f64) {
    let keys: Vec<f64> = items.iter().map(&key).collect();
    if keys.iter().all(|k| !k.is_nan()) {
        let mut idx: Vec<usize> = (0..items.len()).collect();
        idx.sort_by(|&a, &b| keys[a].partial_cmp(&keys[b]).unwrap_or(Ordering::Equal));
        permute(items, &idx);
    } else {
        let mut idx: Vec<usize> = (0..items.len()).collect();
        timsort(&mut idx, &|&a: &usize, &b: &usize| keys[a] < keys[b]);
        permute(items, &idx);
    }
}

/// `items.sort(key=key)` with keys compared by `lt` (Python's `<` on the key objects).
pub fn sort_by_key_lt<T, K>(items: &mut Vec<T>, key: impl Fn(&T) -> K, lt: impl Fn(&K, &K) -> bool) {
    let keys: Vec<K> = items.iter().map(&key).collect();
    let mut idx: Vec<usize> = (0..items.len()).collect();
    timsort(&mut idx, &|&a: &usize, &b: &usize| lt(&keys[a], &keys[b]));
    permute(items, &idx);
}

fn permute<T>(items: &mut Vec<T>, idx: &[usize]) {
    let mut taken: Vec<Option<T>> = items.drain(..).map(Some).collect();
    for &i in idx {
        items.push(taken[i].take().expect("permutation"));
    }
}

/// CPython's `list.sort` algorithm (Objects/listobject.c, `listsort`), comparing with `lt` only.
pub fn timsort<T: Clone>(v: &mut [T], lt: &impl Fn(&T, &T) -> bool) {
    super::timsort::listsort(v, lt);
}
