//! Port of CPython 3.14's `list.sort` (Objects/listobject.c: `list_sort_impl` and its helpers).
//! See `sort.rs`.
//!
//! When `<` is not a total order (NaN keys), the result of a sort depends on exactly which pairs
//! the algorithm compares, so this is the same algorithm, step by step: `count_run` (with the
//! in-place reversal of descending runs and their all-equal sub-runs), `binarysort`,
//! `gallop_left`/`gallop_right`, `merge_lo`/`merge_hi` with the adaptive `min_gallop`, and the
//! powersort merge policy (`powerloop`, `found_new_run`, `merge_force_collapse`). Comparisons go
//! through `lt` only, in the same order and with the same operands as CPython's `ISLT`.
//!
//! `lt` cannot raise, so CPython's `fail` paths (which only exist for exceptions raised by `<`)
//! are not needed; the "comparison is inconsistent" guards (`na == 0`, `nb == 0`) are kept.

/// The maximum number of entries in a MergeState's pending-runs stack.
const MAX_MERGE_PENDING: usize = usize::BITS as usize;

/// When we get into galloping mode, we stay there until both runs win less
/// often than MIN_GALLOP consecutive times.
const MIN_GALLOP: isize = 7;

/// The largest value of minrun.
const MAX_MINRUN: isize = 64;

#[derive(Clone, Copy, Default)]
struct SSlice {
    /// Index of the run's first element.
    base: isize,
    /// length of run
    len: isize,
    /// node "level" for powersort merge strategy
    power: i32,
}

/// `MergeState`.
struct MergeState<T> {
    /// This controls when we get *into* galloping mode.
    min_gallop: isize,
    /// The temporary merge area (`ms->a`).
    a: Vec<T>,
    /// A stack of n pending runs yet to be merged.
    n: usize,
    pending: [SSlice; MAX_MERGE_PENDING],
    listlen: isize,
}

/// `list.sort()` of `v`, comparing with `lt` (`x < y`) only.
pub fn listsort<T: Clone>(v: &mut [T], lt: &impl Fn(&T, &T) -> bool) {
    let saved_ob_size = v.len() as isize;
    // merge_init
    let mut ms: MergeState<T> = MergeState {
        min_gallop: MIN_GALLOP,
        a: Vec::new(),
        n: 0,
        pending: [SSlice::default(); MAX_MERGE_PENDING],
        listlen: saved_ob_size,
    };

    let mut nremaining = saved_ob_size;
    if nremaining < 2 {
        return;
    }

    // March over the array once, left to right, finding natural runs,
    // and extending short natural runs to minrun elements.
    let mut lo: isize = 0;
    let minrun = merge_compute_minrun(nremaining);
    loop {
        // Identify next run.
        let mut n = count_run(v, lo, nremaining, lt);
        // If short, extend to min(minrun, nremaining).
        if n < minrun {
            let force = if nremaining <= minrun { nremaining } else { minrun };
            binarysort(&mut v[lo as usize..(lo + force) as usize], n as usize, lt);
            n = force;
        }
        // Maybe merge pending runs.
        found_new_run(&mut ms, v, n, lt);
        // Push new run on stack.
        assert!(ms.n < MAX_MERGE_PENDING);
        ms.pending[ms.n].base = lo;
        ms.pending[ms.n].len = n;
        ms.n += 1;
        // Advance to find next run.
        lo += n;
        nremaining -= n;
        if nremaining == 0 {
            break;
        }
    }

    merge_force_collapse(&mut ms, v, lt);
    debug_assert!(ms.n == 1);
    debug_assert!(ms.pending[0].len == saved_ob_size);
}

/// `binarysort`: `a[:ok]` is already sorted; a stable binary insertion sort of the rest.
fn binarysort<T: Clone>(a: &mut [T], ok: usize, lt: &impl Fn(&T, &T) -> bool) {
    let n = a.len();
    debug_assert!(ok <= n && 1 <= n && n as isize <= MAX_MINRUN);
    let mut ok = if ok == 0 { 1 } else { ok };
    while ok < n {
        // set L to where a[ok] belongs
        let mut l = 0usize;
        let mut r = ok;
        let pivot = a[ok].clone();
        // Slice invariants. vacuously true at the start:
        // all a[0:L]  <= pivot
        // all a[L:R]     unknown
        // all a[R:ok]  > pivot
        loop {
            let m = (l + r) >> 1;
            if lt(&pivot, &a[m]) {
                r = m;
            } else {
                l = m + 1;
            }
            if l >= r {
                break;
            }
        }
        // a[:L] holds all elements from a[:ok] <= pivot now, so pivot belongs
        // at index L. Slide a[L:ok] to the right a slot to make room for it.
        a[l..=ok].rotate_right(1);
        ok += 1;
    }
}

/// `count_run`: the length of the run beginning at `v[lo]`, spanning no more than `nremaining`
/// elements. A descending run is permuted in place so that it is ascending on return.
fn count_run<T: Clone>(v: &mut [T], lo: isize, nremaining: isize, lt: &impl Fn(&T, &T) -> bool) -> isize {
    let lo = lo as usize;
    let s = &mut v[lo..lo + nremaining as usize];
    // IF_NEXT_LARGER  IFLT(lo[n-1], lo[n])
    // IF_NEXT_SMALLER IFLT(lo[n], lo[n-1])
    let next_smaller = |s: &[T], n: usize| lt(&s[n], &s[n - 1]);
    let next_larger = |s: &[T], n: usize| lt(&s[n - 1], &s[n]);
    let nremaining = nremaining as usize;

    debug_assert!(nremaining > 0);
    // try ascending run first
    let mut n = 1usize;
    while n < nremaining {
        if next_smaller(s, n) {
            break;
        }
        n += 1;
    }
    if n == nremaining {
        return n as isize;
    }
    // lo[n] is strictly less
    // If n is 1 now, then the first compare established it's a descending
    // run, so fall through to the descending case. But if n > 1, there are
    // n elements in an ascending run terminated by the strictly less lo[n].
    // If the first key < lo[n-1], *somewhere* along the way the sequence
    // increased, so we're done (there is no descending run).
    // Else first key >= lo[n-1], which implies that the entire ascending run
    // consists of equal elements. In that case, this is a descending run,
    // and we reverse the all-equal prefix in-place.
    if n > 1 {
        if lt(&s[0], &s[n - 1]) {
            return n as isize;
        }
        s[..n].reverse();
    }
    n += 1; // in all cases it's been established that lo[n] has been resolved

    // Finish descending run. All-squal subruns are reversed in-place on the
    // fly. Their original order will be restored at the end by the whole-slice
    // reversal.
    let mut neq = 0usize;
    // REVERSE_LAST_NEQ
    let reverse_last_neq = |s: &mut [T], n: usize, neq: &mut usize| {
        if *neq != 0 {
            *neq += 1;
            s[n - *neq..n].reverse();
            *neq = 0;
        }
    };
    while n < nremaining {
        if next_smaller(s, n) {
            // This ends the most recent run of equal elements, but still in
            // the "descending" direction.
            reverse_last_neq(s, n, &mut neq);
        } else if next_larger(s, n) {
            // descending run is over
            break;
        } else {
            // not x < y and not y < x implies x == y
            neq += 1;
        }
        n += 1;
    }
    reverse_last_neq(s, n, &mut neq);
    s[..n].reverse(); // transform to ascending run

    // And after reversing, it's possible this can be extended by a
    // naturally increasing suffix; e.g., [3, 2, 3, 4, 1] makes an
    // ascending run from the first 4 elements.
    while n < nremaining {
        if next_smaller(s, n) {
            break;
        }
        n += 1;
    }
    n as isize
}

/// `gallop_left`: the k in 0..=n such that `a[k-1] < key <= a[k]`, starting the search at
/// `hint`.
fn gallop_left<T>(key: &T, a: &[T], hint: isize, lt: &impl Fn(&T, &T) -> bool) -> isize {
    let n = a.len() as isize;
    debug_assert!(n > 0 && hint >= 0 && hint < n);
    let at = |i: isize| &a[i as usize];
    let mut lastofs: isize = 0;
    let mut ofs: isize = 1;
    if lt(at(hint), key) {
        // a[hint] < key -- gallop right, until
        // a[hint + lastofs] < key <= a[hint + ofs]
        let maxofs = n - hint; // &a[n-1] is highest
        while ofs < maxofs {
            if lt(at(hint + ofs), key) {
                lastofs = ofs;
                ofs = (ofs << 1) + 1;
            } else {
                // key <= a[hint + ofs]
                break;
            }
        }
        if ofs > maxofs {
            ofs = maxofs;
        }
        // Translate back to offsets relative to &a[0].
        lastofs += hint;
        ofs += hint;
    } else {
        // key <= a[hint] -- gallop left, until
        // a[hint - ofs] < key <= a[hint - lastofs]
        let maxofs = hint + 1; // &a[0] is lowest
        while ofs < maxofs {
            if lt(at(hint - ofs), key) {
                break;
            }
            // key <= a[hint - ofs]
            lastofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        if ofs > maxofs {
            ofs = maxofs;
        }
        // Translate back to positive offsets relative to &a[0].
        let k = lastofs;
        lastofs = hint - ofs;
        ofs = hint - k;
    }
    debug_assert!(-1 <= lastofs && lastofs < ofs && ofs <= n);
    // Now a[lastofs] < key <= a[ofs], so key belongs somewhere to the
    // right of lastofs but no farther right than ofs.  Do a binary
    // search, with invariant a[lastofs-1] < key <= a[ofs].
    lastofs += 1;
    while lastofs < ofs {
        let m = lastofs + ((ofs - lastofs) >> 1);
        if lt(at(m), key) {
            lastofs = m + 1; // a[m] < key
        } else {
            ofs = m; // key <= a[m]
        }
    }
    debug_assert!(lastofs == ofs); // so a[ofs-1] < key <= a[ofs]
    ofs
}

/// `gallop_right`: like `gallop_left`, but the k such that `a[k-1] <= key < a[k]`.
fn gallop_right<T>(key: &T, a: &[T], hint: isize, lt: &impl Fn(&T, &T) -> bool) -> isize {
    let n = a.len() as isize;
    debug_assert!(n > 0 && hint >= 0 && hint < n);
    let at = |i: isize| &a[i as usize];
    let mut lastofs: isize = 0;
    let mut ofs: isize = 1;
    if lt(key, at(hint)) {
        // key < a[hint] -- gallop left, until
        // a[hint - ofs] <= key < a[hint - lastofs]
        let maxofs = hint + 1; // &a[0] is lowest
        while ofs < maxofs {
            if lt(key, at(hint - ofs)) {
                lastofs = ofs;
                ofs = (ofs << 1) + 1;
            } else {
                // a[hint - ofs] <= key
                break;
            }
        }
        if ofs > maxofs {
            ofs = maxofs;
        }
        // Translate back to positive offsets relative to &a[0].
        let k = lastofs;
        lastofs = hint - ofs;
        ofs = hint - k;
    } else {
        // a[hint] <= key -- gallop right, until
        // a[hint + lastofs] <= key < a[hint + ofs]
        let maxofs = n - hint; // &a[n-1] is highest
        while ofs < maxofs {
            if lt(key, at(hint + ofs)) {
                break;
            }
            // a[hint + ofs] <= key
            lastofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        if ofs > maxofs {
            ofs = maxofs;
        }
        // Translate back to offsets relative to &a[0].
        lastofs += hint;
        ofs += hint;
    }
    debug_assert!(-1 <= lastofs && lastofs < ofs && ofs <= n);
    // Now a[lastofs] <= key < a[ofs], so key belongs somewhere to the
    // right of lastofs but no farther right than ofs.  Do a binary
    // search, with invariant a[lastofs-1] <= key < a[ofs].
    lastofs += 1;
    while lastofs < ofs {
        let m = lastofs + ((ofs - lastofs) >> 1);
        if lt(key, at(m)) {
            ofs = m; // key < a[m]
        } else {
            lastofs = m + 1; // a[m] <= key
        }
    }
    debug_assert!(lastofs == ofs); // so a[ofs-1] <= key < a[ofs]
    ofs
}

/// `merge_lo`: merge the `na` elements starting at `ssa` with the `nb` elements starting at
/// `ssb = ssa + na` in a stable way, in-place. Should have `na <= nb`.
fn merge_lo<T: Clone>(
    ms: &mut MergeState<T>,
    v: &mut [T],
    ssa: isize,
    mut na: isize,
    ssb: isize,
    mut nb: isize,
    lt: &impl Fn(&T, &T) -> bool,
) {
    debug_assert!(na > 0 && nb > 0 && ssa + na == ssb);
    // sortslice_memcpy(&ms->a, 0, &ssa, 0, na)
    ms.a.clear();
    ms.a.extend_from_slice(&v[ssa as usize..(ssa + na) as usize]);
    let mut dest = ssa as usize;
    let mut pa = 0usize; // ssa = ms->a
    let mut pb = ssb as usize;

    macro_rules! succeed {
        () => {{
            // Fail/Succeed: if (na) sortslice_memcpy(&dest, 0, &ssa, 0, na);
            if na != 0 {
                v[dest..dest + na as usize].clone_from_slice(&ms.a[pa..pa + na as usize]);
            }
            return;
        }};
    }
    macro_rules! copy_b {
        () => {{
            debug_assert!(na == 1 && nb > 0);
            // The last element of ssa belongs at the end of the merge.
            for i in 0..nb as usize {
                v[dest + i] = v[pb + i].clone();
            }
            v[dest + nb as usize] = ms.a[pa].clone();
            return;
        }};
    }

    v[dest] = v[pb].clone();
    dest += 1;
    pb += 1;
    nb -= 1;
    if nb == 0 {
        succeed!();
    }
    if na == 1 {
        copy_b!();
    }

    let mut min_gallop = ms.min_gallop;
    loop {
        let mut acount: isize = 0; // # of times A won in a row
        let mut bcount: isize = 0; // # of times B won in a row

        // Do the straightforward thing until (if ever) one run
        // appears to win consistently.
        loop {
            debug_assert!(na > 1 && nb > 0);
            if lt(&v[pb], &ms.a[pa]) {
                v[dest] = v[pb].clone();
                dest += 1;
                pb += 1;
                bcount += 1;
                acount = 0;
                nb -= 1;
                if nb == 0 {
                    succeed!();
                }
                if bcount >= min_gallop {
                    break;
                }
            } else {
                v[dest] = ms.a[pa].clone();
                dest += 1;
                pa += 1;
                acount += 1;
                bcount = 0;
                na -= 1;
                if na == 1 {
                    copy_b!();
                }
                if acount >= min_gallop {
                    break;
                }
            }
        }

        // One run is winning so consistently that galloping may
        // be a huge win.  So try that, and continue galloping until
        // (if ever) neither run appears to be winning consistently
        // anymore.
        min_gallop += 1;
        loop {
            debug_assert!(na > 1 && nb > 0);
            min_gallop -= (min_gallop > 1) as isize;
            ms.min_gallop = min_gallop;
            let k = gallop_right(&v[pb], &ms.a[pa..pa + na as usize], 0, lt);
            acount = k;
            if k != 0 {
                let k = k as usize;
                v[dest..dest + k].clone_from_slice(&ms.a[pa..pa + k]);
                dest += k;
                pa += k;
                na -= k as isize;
                if na == 1 {
                    copy_b!();
                }
                // na==0 is impossible now if the comparison
                // function is consistent, but we can't assume
                // that it is.
                if na == 0 {
                    succeed!();
                }
            }
            v[dest] = v[pb].clone();
            dest += 1;
            pb += 1;
            nb -= 1;
            if nb == 0 {
                succeed!();
            }

            let k = gallop_left(&ms.a[pa], &v[pb..pb + nb as usize], 0, lt);
            bcount = k;
            if k != 0 {
                let k = k as usize;
                // sortslice_memmove(&dest, 0, &ssb, 0, k): dest < pb
                for i in 0..k {
                    v[dest + i] = v[pb + i].clone();
                }
                dest += k;
                pb += k;
                nb -= k as isize;
                if nb == 0 {
                    succeed!();
                }
            }
            v[dest] = ms.a[pa].clone();
            dest += 1;
            pa += 1;
            na -= 1;
            if na == 1 {
                copy_b!();
            }
            if !(acount >= MIN_GALLOP || bcount >= MIN_GALLOP) {
                break;
            }
        }
        min_gallop += 1; // penalize it for leaving galloping mode
        ms.min_gallop = min_gallop;
    }
}

/// `merge_hi`: like `merge_lo`, merging from the right. Should have `na >= nb`.
fn merge_hi<T: Clone>(
    ms: &mut MergeState<T>,
    v: &mut [T],
    ssa: isize,
    mut na: isize,
    ssb: isize,
    mut nb: isize,
    lt: &impl Fn(&T, &T) -> bool,
) {
    debug_assert!(na > 0 && nb > 0 && ssa + na == ssb);
    // dest = ssb; sortslice_advance(&dest, nb-1)
    let mut dest: isize = ssb + nb - 1;
    // sortslice_memcpy(&ms->a, 0, &ssb, 0, nb)
    ms.a.clear();
    ms.a.extend_from_slice(&v[ssb as usize..(ssb + nb) as usize]);
    let basea = ssa;
    // baseb = ms->a (index 0); ssb.keys = ms->a.keys + nb - 1
    let mut pb: isize = nb - 1;
    // sortslice_advance(&ssa, na - 1)
    let mut pa: isize = ssa + na - 1;

    let u = |i: isize| i as usize;

    macro_rules! succeed {
        () => {{
            // if (nb) sortslice_memcpy(&dest, -(nb-1), &baseb, 0, nb);
            if nb != 0 {
                let start = u(dest - (nb - 1));
                v[start..start + u(nb)].clone_from_slice(&ms.a[0..u(nb)]);
            }
            return;
        }};
    }
    macro_rules! copy_a {
        () => {{
            debug_assert!(nb == 1 && na > 0);
            // The first element of ssb belongs at the front of the merge.
            // sortslice_memmove(&dest, 1-na, &ssa, 1-na, na): dest > pa, copy backwards
            for i in (0..na).rev() {
                v[u(dest + 1 - na + i)] = v[u(pa + 1 - na + i)].clone();
            }
            dest -= na;
            pa -= na;
            let _ = pa;
            v[u(dest)] = ms.a[u(pb)].clone();
            return;
        }};
    }

    v[u(dest)] = v[u(pa)].clone();
    dest -= 1;
    pa -= 1;
    na -= 1;
    if na == 0 {
        succeed!();
    }
    if nb == 1 {
        copy_a!();
    }

    let mut min_gallop = ms.min_gallop;
    loop {
        let mut acount: isize = 0; // # of times A won in a row
        let mut bcount: isize = 0; // # of times B won in a row

        // Do the straightforward thing until (if ever) one run
        // appears to win consistently.
        loop {
            debug_assert!(na > 0 && nb > 1);
            if lt(&ms.a[u(pb)], &v[u(pa)]) {
                v[u(dest)] = v[u(pa)].clone();
                dest -= 1;
                pa -= 1;
                acount += 1;
                bcount = 0;
                na -= 1;
                if na == 0 {
                    succeed!();
                }
                if acount >= min_gallop {
                    break;
                }
            } else {
                v[u(dest)] = ms.a[u(pb)].clone();
                dest -= 1;
                pb -= 1;
                bcount += 1;
                acount = 0;
                nb -= 1;
                if nb == 1 {
                    copy_a!();
                }
                if bcount >= min_gallop {
                    break;
                }
            }
        }

        // One run is winning so consistently that galloping may
        // be a huge win.  So try that, and continue galloping until
        // (if ever) neither run appears to be winning consistently
        // anymore.
        min_gallop += 1;
        loop {
            debug_assert!(na > 0 && nb > 1);
            min_gallop -= (min_gallop > 1) as isize;
            ms.min_gallop = min_gallop;
            let k = gallop_right(&ms.a[u(pb)], &v[u(basea)..u(basea + na)], na - 1, lt);
            let k = na - k;
            acount = k;
            if k != 0 {
                dest -= k;
                pa -= k;
                // sortslice_memmove(&dest, 1, &ssa, 1, k): dest > pa, copy backwards
                for i in (1..=k).rev() {
                    v[u(dest + i)] = v[u(pa + i)].clone();
                }
                na -= k;
                if na == 0 {
                    succeed!();
                }
            }
            v[u(dest)] = ms.a[u(pb)].clone();
            dest -= 1;
            pb -= 1;
            nb -= 1;
            if nb == 1 {
                copy_a!();
            }

            let k = gallop_left(&v[u(pa)], &ms.a[0..u(nb)], nb - 1, lt);
            let k = nb - k;
            bcount = k;
            if k != 0 {
                dest -= k;
                pb -= k;
                // sortslice_memcpy(&dest, 1, &ssb, 1, k)
                let (d, s) = (u(dest + 1), u(pb + 1));
                v[d..d + u(k)].clone_from_slice(&ms.a[s..s + u(k)]);
                nb -= k;
                if nb == 1 {
                    copy_a!();
                }
                // nb==0 is impossible now if the comparison
                // function is consistent, but we can't assume
                // that it is.
                if nb == 0 {
                    succeed!();
                }
            }
            v[u(dest)] = v[u(pa)].clone();
            dest -= 1;
            pa -= 1;
            na -= 1;
            if na == 0 {
                succeed!();
            }
            if !(acount >= MIN_GALLOP || bcount >= MIN_GALLOP) {
                break;
            }
        }
        min_gallop += 1; // penalize it for leaving galloping mode
        ms.min_gallop = min_gallop;
    }
}

/// `merge_at`: merge the two runs at stack indices i and i+1.
fn merge_at<T: Clone>(ms: &mut MergeState<T>, v: &mut [T], i: usize, lt: &impl Fn(&T, &T) -> bool) {
    debug_assert!(ms.n >= 2);
    debug_assert!(i == ms.n - 2 || i == ms.n - 3);

    let mut ssa = ms.pending[i].base;
    let mut na = ms.pending[i].len;
    let ssb = ms.pending[i + 1].base;
    let mut nb = ms.pending[i + 1].len;
    debug_assert!(na > 0 && nb > 0);
    debug_assert!(ssa + na == ssb);

    // Record the length of the combined runs; if i is the 3rd-last
    // run now, also slide over the last run (which isn't involved
    // in this merge).  The current run i+1 goes away in any case.
    ms.pending[i].len = na + nb;
    if i + 3 == ms.n {
        ms.pending[i + 1] = ms.pending[i + 2];
    }
    ms.n -= 1;

    // Where does b start in a?  Elements in a before that can be
    // ignored (already in place).
    let k = gallop_right(&v[ssb as usize], &v[ssa as usize..(ssa + na) as usize], 0, lt);
    ssa += k;
    na -= k;
    if na == 0 {
        return;
    }

    // Where does a end in b?  Elements in b after that can be
    // ignored (already in place).
    nb = gallop_left(&v[(ssa + na - 1) as usize], &v[ssb as usize..(ssb + nb) as usize], nb - 1, lt);
    if nb <= 0 {
        return;
    }

    // Merge what remains of the runs, using a temp array with
    // min(na, nb) elements.
    if na <= nb { merge_lo(ms, v, ssa, na, ssb, nb, lt) } else { merge_hi(ms, v, ssa, na, ssb, nb, lt) }
}

/// `powerloop`: the "power" of the first of two adjacent runs (s1, n1) and (s1+n1, n2) in a
/// list of length n.
fn powerloop(s1: isize, n1: isize, n2: isize, n: isize) -> i32 {
    let mut result = 0;
    debug_assert!(s1 >= 0);
    debug_assert!(n1 > 0 && n2 > 0);
    debug_assert!(s1 + n1 + n2 <= n);
    // midpoints a and b:
    // a = s1 + n1/2
    // b = s1 + n1 + n2/2 = a + (n1 + n2)/2
    //
    // Those may not be integers, though, because of the "/2". So we work with
    // 2*a and 2*b instead, which are necessarily integers. It makes no
    // difference to the outcome, since the bits in the expansion of (2*i)/n
    // are merely shifted one position from those of i/n.
    let mut a = 2 * s1 + n1; // 2*a
    let mut b = a + n1 + n2; // 2*b
    // Emulate a/n and b/n one bit a time, until bits differ.
    loop {
        result += 1;
        if a >= n {
            // both quotient bits are 1
            a -= n;
            b -= n;
        } else if b >= n {
            // a/n bit is 0, b/n bit is 1
            break;
        } // else both quotient bits are 0
        a <<= 1;
        b <<= 1;
    }
    result
}

/// `found_new_run`: the next run has been identified, of length n2. Apply the "powersort"
/// merge strategy. The caller pushes the new run on the stack.
fn found_new_run<T: Clone>(ms: &mut MergeState<T>, v: &mut [T], n2: isize, lt: &impl Fn(&T, &T) -> bool) {
    if ms.n != 0 {
        let s1 = ms.pending[ms.n - 1].base; // start index
        let n1 = ms.pending[ms.n - 1].len;
        let power = powerloop(s1, n1, n2, ms.listlen);
        while ms.n > 1 && ms.pending[ms.n - 2].power > power {
            merge_at(ms, v, ms.n - 2, lt);
        }
        debug_assert!(ms.n < 2 || ms.pending[ms.n - 2].power < power);
        ms.pending[ms.n - 1].power = power;
    }
}

/// `merge_force_collapse`: merge all runs on the stack until only one remains.
fn merge_force_collapse<T: Clone>(ms: &mut MergeState<T>, v: &mut [T], lt: &impl Fn(&T, &T) -> bool) {
    while ms.n > 1 {
        let mut n = ms.n - 2;
        if n > 0 && ms.pending[n - 1].len < ms.pending[n + 1].len {
            n -= 1;
        }
        merge_at(ms, v, n, lt);
    }
}

/// `merge_compute_minrun`.
fn merge_compute_minrun(mut n: isize) -> isize {
    let mut r = 0; // becomes 1 if any 1 bits are shifted off
    debug_assert!(n >= 0);
    while n >= MAX_MINRUN {
        r |= n & 1;
        n >>= 1;
    }
    n + r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted_indices(keys: &[f64]) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..keys.len()).collect();
        listsort(&mut idx, &|&a: &usize, &b: &usize| keys[a] < keys[b]);
        idx
    }

    #[test]
    fn total_order_is_stable_sort() {
        // A simple LCG; compare with Rust's stable sort on totally ordered keys.
        let mut x: u64 = 12345;
        for n in [0usize, 1, 2, 5, 63, 64, 65, 100, 257, 1000, 4097] {
            for distinct in [2u64, 10, 1000] {
                let keys: Vec<f64> = (0..n)
                    .map(|_| {
                        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        ((x >> 33) % distinct) as f64
                    })
                    .collect();
                let got = sorted_indices(&keys);
                let mut want: Vec<usize> = (0..n).collect();
                want.sort_by(|&a, &b| keys[a].partial_cmp(&keys[b]).unwrap());
                assert_eq!(got, want, "n={n} distinct={distinct}");
            }
        }
    }

    #[test]
    fn nan_keys_small() {
        // Python: sorted(range(5), key=[3.0, nan, 1.0, nan, 2.0].__getitem__)
        let nan = f64::NAN;
        assert_eq!(sorted_indices(&[3.0, nan, 1.0, nan, 2.0]), vec![0, 1, 2, 3, 4]);
        let keys = [3.0, 1.0, nan, 2.0, 0.5, nan, 0.0, 4.0];
        assert_eq!(sorted_indices(&keys), vec![4, 5, 6, 1, 2, 3, 0, 7]);
    }
}
