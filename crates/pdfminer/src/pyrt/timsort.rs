//! Port of CPython's `listsort` (Objects/listobject.c). See `sort.rs`.

/// Placeholder: binary insertion sort (what CPython does for lists shorter than 64 items, which
/// pdfminer's NaN-keyed sorts virtually always are). Replaced by the full port in a later step.
pub fn listsort<T: Clone>(v: &mut [T], lt: &impl Fn(&T, &T) -> bool) {
    binarysort(v, 0, lt);
}

/// CPython's `binarysort`: v[..start] is already sorted; insert the rest one by one.
fn binarysort<T: Clone>(v: &mut [T], start: usize, lt: &impl Fn(&T, &T) -> bool) {
    let n = v.len();
    let mut start = if start == 0 && n > 0 { 1 } else { start };
    while start < n {
        let pivot = v[start].clone();
        let (mut l, mut r) = (0usize, start);
        while l < r {
            let p = l + ((r - l) >> 1);
            if lt(&pivot, &v[p]) {
                r = p;
            } else {
                l = p + 1;
            }
        }
        let mut p = start;
        while p > l {
            v[p] = v[p - 1].clone();
            p -= 1;
        }
        v[l] = pivot;
        start += 1;
    }
}
