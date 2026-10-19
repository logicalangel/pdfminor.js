//! Port of `pdfminer/data_structures.py`.

use crate::pdfdocument::RECURSION_LIMIT;
use crate::pdftypes::{dict_value, int_value, list_value};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::value::{DictRef, Value};
use crate::settings;
use crate::utils::choplist;

/// A PDF number tree.
///
/// See Section 3.8.6 of the PDF Reference.
pub struct NumberTree {
    pub _obj: DictRef,
    pub nums: Option<Vec<Value>>,
    pub kids: Option<Vec<Value>>,
    pub limits: Option<Vec<Value>>,
}

impl NumberTree {
    /// `NumberTree(obj)`.
    pub fn new(obj: &Value) -> PyResult<Self> {
        let _obj = dict_value(obj)?;
        let mut t = NumberTree { _obj, nums: None, kids: None, limits: None };
        let get = |k: &str| t._obj.borrow().get(k).cloned();
        let nums = get("Nums");
        let kids = get("Kids");
        let limits = get("Limits");
        if let Some(v) = nums {
            t.nums = Some(list_value(&v)?);
        }
        if let Some(v) = kids {
            t.kids = Some(list_value(&v)?);
        }
        if let Some(v) = limits {
            t.limits = Some(list_value(&v)?);
        }
        Ok(t)
    }

    /// `_parse()`. `depth` counts the nested calls Python makes for the kids (a cyclic tree ends
    /// in `RecursionError`, as in Python).
    pub fn _parse(&self) -> PyResult<Vec<(i64, Value)>> {
        self.parse_depth(0)
    }

    fn parse_depth(&self, depth: usize) -> PyResult<Vec<(i64, Value)>> {
        if depth > RECURSION_LIMIT {
            return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
        }
        let mut items = Vec::new();
        if let Some(nums) = self.nums.as_ref().filter(|n| !n.is_empty()) {
            // Leaf node
            for pair in choplist(2, nums.iter().cloned()) {
                items.push((int_value(&pair[0])?, pair[1].clone()));
            }
        }
        if let Some(kids) = self.kids.as_ref().filter(|k| !k.is_empty()) {
            // Root or intermediate node
            for child_ref in kids {
                items.extend(NumberTree::new(child_ref)?.parse_depth(depth + 1)?);
            }
        }
        Ok(items)
    }

    /// The `values` property.
    pub fn values(&self) -> PyResult<Vec<(i64, Value)>> {
        let mut values = self._parse()?;
        if settings::STRICT {
            if !values.windows(2).all(|w| w[0].0 <= w[1].0) {
                return Err(PyErr::new(ExcKind::PDFSyntaxError, "Number tree elements are out of order"));
            }
        } else {
            // Stable, like `list.sort(key=lambda t: t[0])`.
            values.sort_by_key(|t| t.0);
        }
        Ok(values)
    }
}
