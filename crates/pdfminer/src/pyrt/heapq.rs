//! A literal port of CPython's `heapq` (heappush, heappop, heapify), ordered only by a `lt`
//! closure. Using the same algorithm (not `BinaryHeap`) keeps the pop order identical to Python's
//! even when keys tie or contain NaN.

/// `heapq.heappush(heap, item)`.
pub fn heappush<T>(heap: &mut Vec<T>, item: T, lt: &impl Fn(&T, &T) -> bool) {
    heap.push(item);
    let n = heap.len() - 1;
    siftdown(heap, 0, n, lt);
}

/// `heapq.heappop(heap)`; `None` for an empty heap (Python raises `IndexError`).
pub fn heappop<T>(heap: &mut Vec<T>, lt: &impl Fn(&T, &T) -> bool) -> Option<T> {
    let lastelt = heap.pop()?;
    if !heap.is_empty() {
        let returnitem = std::mem::replace(&mut heap[0], lastelt);
        siftup(heap, 0, lt);
        return Some(returnitem);
    }
    Some(lastelt)
}

/// `heapq.heapify(x)`.
pub fn heapify<T>(x: &mut [T], lt: &impl Fn(&T, &T) -> bool) {
    let n = x.len();
    for i in (0..n / 2).rev() {
        siftup(x, i, lt);
    }
}

// 'heap' is a heap at all indices >= startpos, except possibly for pos. pos is the index of a
// leaf with a possibly out-of-order value. Restore the heap invariant.
fn siftdown<T>(heap: &mut [T], startpos: usize, mut pos: usize, lt: &impl Fn(&T, &T) -> bool) {
    while pos > startpos {
        let parentpos = (pos - 1) >> 1;
        if lt(&heap[pos], &heap[parentpos]) {
            heap.swap(pos, parentpos);
            pos = parentpos;
            continue;
        }
        break;
    }
}

fn siftup<T>(heap: &mut [T], mut pos: usize, lt: &impl Fn(&T, &T) -> bool) {
    let endpos = heap.len();
    let startpos = pos;
    // Bubble up the smaller child until hitting a leaf.
    let mut childpos = 2 * pos + 1;
    while childpos < endpos {
        let rightpos = childpos + 1;
        if rightpos < endpos && !lt(&heap[childpos], &heap[rightpos]) {
            childpos = rightpos;
        }
        heap.swap(pos, childpos);
        pos = childpos;
        childpos = 2 * pos + 1;
    }
    // The leaf at pos is empty now. Put newitem there, and bubble it up to its final resting
    // place (by sifting its parents down).
    siftdown(heap, startpos, pos, lt);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_in_order() {
        let lt = |a: &i32, b: &i32| a < b;
        let mut h = vec![5, 3, 8, 1, 9, 2];
        heapify(&mut h, &lt);
        heappush(&mut h, 0, &lt);
        let mut out = vec![];
        while let Some(x) = heappop(&mut h, &lt) {
            out.push(x);
        }
        assert_eq!(out, vec![0, 1, 2, 3, 5, 8, 9]);
    }
}
