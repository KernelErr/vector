pub mod convert;
pub mod target;

use crate::value::convert::regex_to_bytes;
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use core::fmt;
use lookup::{Field, FieldBuf, Look, Lookup, LookupBuf, Segment, SegmentBuf};
use ordered_float::NotNan;
use regex::Regex;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use snafu::Snafu;
use std::collections::BTreeMap;
use std::fmt::{Debug, Write};
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::Instrument;

// --- TODO LIST ----
//TODO: VRL uses standard `PartialEq`, but Vector has odd f64 eq requirements
//TODO: index insert behavior is different for negative vs positive. Negative fills in holes, Positive silently fails

#[derive(Debug, Snafu)]
pub enum ValueError {
    #[snafu(display(
        "Cannot insert value nested inside primitive located at {}. {} was the original target.",
        primitive_at,
        original_target
    ))]
    PrimitiveDescent {
        primitive_at: LookupBuf,
        original_target: LookupBuf,
        original_value: Option<Value>,
    },
    #[snafu(display("Lookup Error: {}", source))]
    LookupError { source: lookup::LookupError },
    #[snafu(display("Empty coalesce subsegment found."))]
    EmptyCoalesceSubSegment,
    #[snafu(display("Cannot remove self."))]
    RemovingSelf,
}

impl From<lookup::LookupError> for ValueError {
    fn from(v: lookup::LookupError) -> Self {
        Self::LookupError { source: v }
    }
}

//TODO: What is PartialOrd used for?
#[derive(Clone, Debug /*, PartialOrd*/)]
pub enum Value {
    Bytes(Bytes),
    Integer(i64),
    Float(NotNan<f64>),
    Boolean(bool),
    Timestamp(DateTime<Utc>),
    Map(BTreeMap<String, Value>),
    Array(Vec<Value>),
    /// In the context of Vector, this is treated the same as Bytes. It means something more in VRL
    Regex(Regex),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bytes(val) => write!(
                f,
                r#""{}""#,
                String::from_utf8_lossy(val)
                    .replace(r#"\"#, r#"\\"#)
                    .replace(r#"""#, r#"\""#)
                    .replace("\n", r#"\n"#)
            ),
            Value::Integer(val) => write!(f, "{}", val),
            Value::Float(val) => write!(f, "{}", val),
            Value::Boolean(val) => write!(f, "{}", val),
            Value::Map(map) => {
                let joined = map
                    .iter()
                    .map(|(key, val)| format!(r#""{}": {}"#, key, val))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{{ {} }}", joined)
            }
            Value::Array(array) => {
                let joined = array
                    .iter()
                    .map(|val| format!("{}", val))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "[{}]", joined)
            }
            Value::Timestamp(val) => {
                write!(f, "t'{}'", val.to_rfc3339_opts(SecondsFormat::AutoSi, true))
            }
            Value::Regex(regex) => write!(f, "r'{}'", regex.to_string()),
            Value::Null => write!(f, "null"),
        }
    }
}

impl Value {
    // TODO: return Cow
    pub fn to_string_lossy(&self) -> String {
        match self {
            Value::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Value::Regex(regex) => regex.to_string(),
            Value::Timestamp(timestamp) => timestamp_to_string(timestamp),
            Value::Integer(num) => format!("{}", num),
            Value::Float(num) => format!("{}", num),
            Value::Boolean(b) => format!("{}", b),
            Value::Map(map) => serde_json::to_string(map).expect("Cannot serialize map"),
            Value::Array(arr) => serde_json::to_string(arr).expect("Cannot serialize array"),
            Value::Null => "<null>".to_string(),
        }
    }

    pub fn kind_str(&self) -> &str {
        match self {
            Value::Bytes(_) => "string",
            // Regex intentionally pretends to be "Bytes"
            Value::Regex(_) => "string",
            Value::Timestamp(_) => "timestamp",
            Value::Integer(_) => "integer",
            Value::Float(_) => "float",
            Value::Boolean(_) => "boolean",
            Value::Map(_) => "map",
            Value::Array(_) => "array",
            Value::Null => "null",
        }
    }

    /// Returns self as a mutable `BTreeMap<String, Value>`
    pub fn as_map_mut(&mut self) -> Option<&mut BTreeMap<String, Value>> {
        match self {
            Value::Map(ref mut m) => Some(m),
            _ => None,
        }
    }

    pub fn into_bytes(self) -> Bytes {
        self.as_bytes()
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match &self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<NotNan<f64>> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    pub fn is_float(&self) -> bool {
        match self {
            Self::Float(_) => true,
            _ => false,
        }
    }

    pub fn is_bytes(&self) -> bool {
        match self {
            Self::Bytes(_) => true,
            _ => false,
        }
    }

    pub fn is_null(&self) -> bool {
        match self {
            Self::Null => true,
            _ => false,
        }
    }

    pub fn is_integer(&self) -> bool {
        match self {
            Self::Integer(_) => true,
            _ => false,
        }
    }

    pub fn into_map(self) -> Option<BTreeMap<String, Value>> {
        match self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_timestamp(&self) -> Option<&DateTime<Utc>> {
        match &self {
            Value::Timestamp(ts) => Some(ts),
            _ => None,
        }
    }

    //TODO: rename to "as_bytes"
    pub fn as_bytes2(&self) -> Option<&Bytes> {
        match self {
            Value::Bytes(bytes) => Some(bytes), // cloning a Bytes is cheap
            _ => None,
        }
    }

    //TODO: rename, this doesn't match the other "as_x" functions (maybe coerce_to_bytes?)
    pub fn as_bytes(&self) -> Bytes {
        match self {
            Value::Bytes(bytes) => bytes.clone(), // cloning a Bytes is cheap
            Value::Regex(regex) => regex_to_bytes(regex),
            Value::Timestamp(timestamp) => Bytes::from(timestamp_to_string(timestamp)),
            Value::Integer(num) => Bytes::from(format!("{}", num)),
            Value::Float(num) => Bytes::from(format!("{}", num)),
            Value::Boolean(b) => Bytes::from(format!("{}", b)),
            Value::Map(map) => Bytes::from(serde_json::to_vec(map).expect("Cannot serialize map")),
            Value::Array(arr) => {
                Bytes::from(serde_json::to_vec(arr).expect("Cannot serialize array"))
            }
            Value::Null => Bytes::from("<null>"),
        }
    }

    /// Insert the current value into a given path.
    ///
    /// For example, given the path `.foo.bar` and value `true`, the return
    /// value would be an object representing `{ "foo": { "bar": true } }`.
    pub fn at_path(mut self, path: &LookupBuf) -> Self {
        for segment in path.as_segments().iter().rev() {
            match segment {
                SegmentBuf::Field(FieldBuf { name, .. }) => {
                    let mut map = BTreeMap::default();
                    map.insert(name.as_str().to_owned(), self);
                    self = Value::Map(map);
                }
                SegmentBuf::Coalesce(fields) => {
                    let field = fields.last().unwrap();
                    let mut map = BTreeMap::default();
                    map.insert(field.as_str().to_owned(), self);
                    self = Value::Map(map);
                }
                SegmentBuf::Index(index) => {
                    let mut array = vec![];

                    if *index > 0 {
                        array.resize(*index as usize, Value::Null);
                    }

                    array.push(self);
                    self = Value::Array(array);
                }
            }
        }

        self
    }

    /// Remove a value that exists at a given lookup.
    ///
    /// Setting `prune` to true will also remove the entries of maps and arrays that are emptied.
    ///
    /// ```rust
    /// use value::Value;
    /// use lookup::{Look, Lookup};
    /// use std::collections::BTreeMap;
    ///
    /// let mut inner_map = Value::from(BTreeMap::default());
    /// inner_map.insert("baz", 1);
    ///
    /// let mut map = Value::from(BTreeMap::default());
    /// map.insert("bar", inner_map.clone());
    /// map.insert("star", inner_map.clone());
    ///
    /// assert_eq!(map.remove("bar", true).unwrap(), Some(Value::from(inner_map)));
    ///
    /// let lookup_key = Lookup::from_str("star.baz").unwrap();
    /// assert_eq!(map.remove(lookup_key, true).unwrap(), Some(Value::from(1)));
    /// assert!(!map.contains("star"));
    /// ```
    #[allow(clippy::too_many_lines)]
    #[allow(clippy::missing_errors_doc)]
    pub fn remove<'a>(
        &mut self,
        lookup: impl Into<Lookup<'a>> + Debug,
        prune: bool,
    ) -> std::result::Result<Option<Value>, ValueError> {
        let mut working_lookup = lookup.into();
        let span = trace_span!("remove", lookup = %working_lookup, %prune);
        let _guard = span.enter();

        let this_segment = working_lookup.pop_front();

        let retval = match (this_segment, &mut *self) {
            // We've met an end without finding a value. (Terminus nodes on arrays/maps detected prior)
            (None, _item) => {
                trace!("Found nothing to remove.");
                Ok(None)
            }
            // This is just not allowed!
            (Some(segment), Value::Boolean(_))
            | (Some(segment), Value::Bytes(_))
            | (Some(segment), Value::Regex(_))
            | (Some(segment), Value::Timestamp(_))
            | (Some(segment), Value::Float(_))
            | (Some(segment), Value::Integer(_))
            | (Some(segment), Value::Null) => {
                if working_lookup.is_empty() {
                    trace!("Cannot remove self. Caller must remove.");
                    Err(ValueError::RemovingSelf)
                } else {
                    trace!("Encountered descent into a primitive.");
                    Err(ValueError::PrimitiveDescent {
                        primitive_at: LookupBuf::default(),
                        original_target: {
                            let mut l = LookupBuf::from(segment.clone().into_buf());
                            l.extend(working_lookup.into_buf());
                            l
                        },
                        original_value: None,
                    })
                }
            }
            // Descend into a coalesce
            (Some(Segment::Coalesce(sub_segments)), value) => {
                // Creating a needle with a back out of the loop is very important.
                let mut needle = None;
                for sub_segment in sub_segments {
                    let mut lookup = Lookup::from(sub_segment);
                    // Notice we cannot take multiple mutable borrows in a loop, so we must pay the
                    // contains cost extra. It's super unfortunate, hopefully future work can solve this.
                    lookup.extend(working_lookup.clone()); // We need to include the rest of the removal.
                    if value.contains(lookup.clone()) {
                        needle = Some(lookup);
                        break;
                    }
                }
                match needle {
                    Some(needle) => value.remove(needle, prune),
                    None => Ok(None),
                }
            }
            // Descend into a map
            (Some(Segment::Field(Field { name, .. })), Value::Map(map)) => {
                if working_lookup.is_empty() {
                    Ok(map.remove(name))
                } else {
                    let mut inner_is_empty = false;
                    let retval = match map.get_mut(name) {
                        Some(inner) => {
                            let ret = inner.remove(working_lookup.clone(), prune);
                            if inner.is_empty() {
                                inner_is_empty = true;
                            };
                            ret
                        }
                        None => Ok(None),
                    };
                    if inner_is_empty && prune {
                        map.remove(name);
                    }
                    retval
                }
            }
            (Some(Segment::Index(_)), Value::Map(_))
            | (Some(Segment::Field { .. }), Value::Array(_)) => Ok(None),
            // Descend into an array
            (Some(Segment::Index(i)), Value::Array(array)) => {
                let index = if i.is_negative() {
                    if i.abs() > array.len() as isize {
                        // The index is before the start of the array.
                        return Ok(None);
                    }
                    (array.len() as isize + i) as usize
                } else {
                    i as usize
                };

                if working_lookup.is_empty() {
                    // We don't **actually** want to remove the index, we just
                    // want to swap it with a null.
                    if array.len() > index {
                        Ok(Some(array.remove(index)))
                    } else {
                        Ok(None)
                    }
                } else {
                    let mut inner_is_empty = false;
                    let retval = match array.get_mut(index) {
                        Some(inner) => {
                            let ret = inner.remove(working_lookup.clone(), prune);
                            if inner.is_empty() {
                                inner_is_empty = true;
                            }
                            ret
                        }
                        None => Ok(None),
                    };
                    if inner_is_empty && prune {
                        array.remove(index);
                    }
                    retval
                }
            }
        };

        retval
    }

    /// Return if the node is empty, that is, it is an array or map with no items.
    ///
    /// ```rust
    /// use vector_core::event::Value;
    /// use std::collections::BTreeMap;
    ///
    /// let val = Value::from(1);
    /// assert_eq!(val.is_empty(), false);
    ///
    /// let mut val = Value::from(Vec::<Value>::default());
    /// assert_eq!(val.is_empty(), true);
    /// val.insert(0, 1);
    /// assert_eq!(val.is_empty(), false);
    /// val.insert(3, 1);
    /// assert_eq!(val.is_empty(), false);
    ///
    /// let mut val = Value::from(BTreeMap::default());
    /// assert_eq!(val.is_empty(), true);
    /// val.insert("foo", 1);
    /// assert_eq!(val.is_empty(), false);
    /// val.insert("bar", 2);
    /// assert_eq!(val.is_empty(), false);
    /// ```
    pub fn is_empty(&self) -> bool {
        match &self {
            Value::Boolean(_)
            | Value::Bytes(_)
            | Value::Regex(_)
            | Value::Timestamp(_)
            | Value::Float(_)
            | Value::Integer(_) => false,
            Value::Null => true,
            Value::Map(v) => v.is_empty(),
            Value::Array(v) => v.is_empty(),
        }
    }

    /// Determine if the lookup is contained within the value.
    ///
    /// ```rust
    /// use vector_core::event::Value;
    /// use lookup::Lookup;
    /// use std::collections::BTreeMap;
    ///
    /// let mut inner_map = Value::from(BTreeMap::default());
    /// inner_map.insert("baz", 1);
    ///
    /// let mut map = Value::from(BTreeMap::default());
    /// map.insert("bar", inner_map.clone());
    ///
    /// assert!(map.contains("bar"));
    ///
    /// let lookup_key = Lookup::from_str("bar.baz").unwrap();
    /// assert!(map.contains(lookup_key));
    /// ```
    #[instrument(level = "trace", skip(self))]
    pub fn contains<'a>(&self, lookup: impl Into<Lookup<'a>> + Debug) -> bool {
        self.get(lookup.into()).unwrap_or(None).is_some()
    }

    /// Get an immutable borrow of the value by lookup.
    ///
    /// ```rust
    /// use value::Value;
    /// use lookup::Lookup;
    /// use std::collections::BTreeMap;
    ///
    /// let mut inner_map = Value::from(BTreeMap::default());
    /// inner_map.insert("baz", 1);
    ///
    /// let mut map = Value::from(BTreeMap::default());
    /// map.insert("bar", inner_map.clone());
    ///
    /// assert_eq!(map.get("bar").unwrap(), Some(&Value::from(inner_map)));
    ///
    /// let lookup_key = Lookup::from_str("bar.baz").unwrap();
    /// assert_eq!(map.get(lookup_key).unwrap(), Some(&Value::from(1)));
    /// ```
    #[allow(clippy::missing_errors_doc)]
    pub fn get<'a>(
        &self,
        lookup: impl Into<Lookup<'a>> + Debug,
    ) -> std::result::Result<Option<&Value>, ValueError> {
        let mut working_lookup = lookup.into();
        let span = trace_span!("get", lookup = %working_lookup);
        let _guard = span.enter();

        let this_segment = working_lookup.pop_front();
        match (this_segment, self) {
            // We've met an end and found our value.
            (None, item) => Ok(Some(item)),
            // Descend into a coalesce
            (Some(Segment::Coalesce(sub_segments)), value) => {
                // Creating a needle with a back out of the loop is very important.
                let mut needle = None;
                for sub_segment in sub_segments {
                    let mut lookup = Lookup::from(sub_segment);
                    // Notice we cannot take multiple mutable borrows in a loop, so we must pay the
                    // contains cost extra. It's super unfortunate, hopefully future work can solve this.
                    lookup.extend(working_lookup.clone()); // We need to include the rest of the get.
                    if value.contains(lookup.clone()) {
                        needle = Some(lookup);
                        break;
                    }
                }
                match needle {
                    Some(needle) => value.get(needle),
                    None => Ok(None),
                }
            }
            // Descend into a map
            (Some(Segment::Field(Field { name, .. })), Value::Map(map)) => match map.get(name) {
                Some(inner) => inner.get(working_lookup.clone()),
                None => Ok(None),
            },
            (Some(Segment::Index(_)), Value::Map(_)) => Ok(None),
            // Descend into an array
            (Some(Segment::Index(i)), Value::Array(array)) => {
                let index = if i.is_negative() {
                    if i.abs() > array.len() as isize {
                        // The index is before the start of the array.
                        return Ok(None);
                    }
                    (array.len() as isize + i) as usize
                } else {
                    i as usize
                };

                match array.get(index) {
                    Some(inner) => inner.get(working_lookup.clone()),
                    None => Ok(None),
                }
            }
            (Some(Segment::Field(Field { .. })), Value::Array(_)) => {
                trace!("Mismatched field trying to access array.");
                Ok(None)
            }
            // This is just not allowed!
            (Some(_s), Value::Boolean(_))
            | (Some(_s), Value::Bytes(_))
            | (Some(_s), Value::Regex(_))
            | (Some(_s), Value::Timestamp(_))
            | (Some(_s), Value::Float(_))
            | (Some(_s), Value::Integer(_))
            | (Some(_s), Value::Null) => {
                trace!("Mismatched primitive field while trying to use segment.");
                Ok(None)
            }
        }
    }

    /// Insert a value at a given lookup.
    ///
    /// ```rust
    /// use vector_core::event::Value;
    /// use lookup::Lookup;
    /// use std::collections::BTreeMap;
    ///
    /// let mut inner_map = Value::from(BTreeMap::default());
    /// inner_map.insert("baz", 1);
    ///
    /// let mut map = Value::from(BTreeMap::default());
    /// map.insert("bar", inner_map.clone());
    /// map.insert("star", inner_map.clone());
    ///
    /// assert!(map.contains("bar"));
    /// assert!(map.contains(Lookup::from_str("star.baz").unwrap()));
    /// ```
    #[allow(clippy::missing_errors_doc)]
    pub fn insert(
        &mut self,
        lookup: impl Into<LookupBuf> + Debug,
        value: impl Into<Value> + Debug,
    ) -> std::result::Result<Option<Value>, ValueError> {
        let mut working_lookup: LookupBuf = lookup.into();
        let value = value.into();
        let span = trace_span!("insert", lookup = %working_lookup);
        let _guard = span.enter();

        let this_segment = working_lookup.pop_front();
        match (this_segment, self) {
            // We've met an end and found our value.
            (None, item) => {
                let mut value = value;
                core::mem::swap(&mut value, item);
                Ok(Some(value))
            }
            // This is just not allowed and should not occur.
            // The top level insert will always be a map (or an array in tests).
            // Then for further descents into the lookup, in the `insert_map` function
            // if the type is one of the following, the field is modified to be a map.
            (Some(segment), Value::Boolean(_))
            | (Some(segment), Value::Bytes(_))
            | (Some(segment), Value::Regex(_))
            | (Some(segment), Value::Timestamp(_))
            | (Some(segment), Value::Float(_))
            | (Some(segment), Value::Integer(_))
            | (Some(segment), Value::Null) => {
                trace!("Encountered descent into a primitive.");
                Err(ValueError::PrimitiveDescent {
                    primitive_at: LookupBuf::default(),
                    original_target: {
                        let mut l = LookupBuf::from(segment);
                        l.extend(working_lookup);
                        l
                    },
                    original_value: Some(value),
                })
            }
            // Descend into a coalesce
            (Some(SegmentBuf::Coalesce(sub_segments)), sub_value) => {
                Value::insert_coalesce(sub_segments, &working_lookup, sub_value, value)
            }
            // Descend into a map
            (
                Some(SegmentBuf::Field(FieldBuf {
                    ref name,
                    ref requires_quoting,
                })),
                Value::Map(ref mut map),
            ) => Value::insert_map(name, *requires_quoting, working_lookup, map, value),
            (Some(SegmentBuf::Index(_)), Value::Map(_)) => {
                trace!("Mismatched index trying to access map.");
                Ok(None)
            }
            // Descend into an array
            (Some(SegmentBuf::Index(i)), Value::Array(ref mut array)) => {
                Value::insert_array(i, working_lookup, array, value)
            }
            (Some(SegmentBuf::Field(FieldBuf { .. })), Value::Array(_)) => {
                trace!("Mismatched field trying to access array.");
                Ok(None)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn insert_array(
        i: isize,
        mut working_lookup: LookupBuf,
        array: &mut Vec<Value>,
        value: Value,
    ) -> std::result::Result<Option<Value>, ValueError> {
        let index = if i.is_negative() {
            array.len() as isize + i
        } else {
            i
        };

        let item = if index.is_negative() {
            // A negative index is greater than the length of the array, so we
            // are trying to set an index that doesn't yet exist.
            None
        } else {
            array.get_mut(index as usize)
        };

        if let Some(inner) = item {
            if let Some(next_segment) = working_lookup.get(0) {
                Value::correct_type(inner, next_segment);
            }

            inner.insert(working_lookup, value).map_err(|mut e| {
                if let ValueError::PrimitiveDescent {
                    original_target,
                    primitive_at,
                    original_value: _,
                } = &mut e
                {
                    let segment = SegmentBuf::Index(i);
                    original_target.push_front(segment.clone());
                    primitive_at.push_front(segment);
                };
                e
            })
        } else {
            if i.is_negative() {
                // Resizing for a negative index must resize to the left.
                // Setting x[-4] to true for an array [0,1] must end up with
                // [true, null, 0, 1]
                let abs = i.abs() as usize - 1;
                let len = array.len();

                array.resize(abs, Value::Null);
                array.rotate_right(abs - len);
            } else {
                // Fill the vector to the index.
                array.resize(i as usize, Value::Null);
            }
            let mut retval = Ok(None);
            let next_val = match working_lookup.get(0) {
                Some(SegmentBuf::Index(next_len)) => {
                    let mut inner = Value::Array(Vec::with_capacity(next_len.abs() as usize));
                    retval = inner.insert(working_lookup, value).map_err(|mut e| {
                        if let ValueError::PrimitiveDescent {
                            original_target,
                            primitive_at,
                            original_value: _,
                        } = &mut e
                        {
                            let segment = SegmentBuf::Index(i);
                            original_target.push_front(segment.clone());
                            primitive_at.push_front(segment);
                        };
                        e
                    });
                    inner
                }
                Some(SegmentBuf::Field(FieldBuf {
                    name,
                    requires_quoting,
                })) => {
                    let mut inner = Value::Map(Default::default());
                    let name = name.clone(); // This is for navigating an ownership issue in the error stack reporting.
                    let requires_quoting = *requires_quoting; // This is for navigating an ownership issue in the error stack reporting.
                    retval = inner.insert(working_lookup, value).map_err(|mut e| {
                        if let ValueError::PrimitiveDescent {
                            original_target,
                            primitive_at,
                            original_value: _,
                        } = &mut e
                        {
                            let segment = SegmentBuf::Field(FieldBuf {
                                name,
                                requires_quoting,
                            });
                            original_target.push_front(segment.clone());
                            primitive_at.push_front(segment);
                        };
                        e
                    });
                    inner
                }
                Some(SegmentBuf::Coalesce(set)) => match set.get(0) {
                    None => return Err(ValueError::EmptyCoalesceSubSegment),
                    Some(_) => {
                        let mut inner = Value::Map(Default::default());
                        let set = SegmentBuf::Coalesce(set.clone());
                        retval = inner.insert(working_lookup, value).map_err(|mut e| {
                            if let ValueError::PrimitiveDescent {
                                original_target,
                                primitive_at,
                                original_value: _,
                            } = &mut e
                            {
                                original_target.push_front(set.clone());
                                primitive_at.push_front(set.clone());
                            };
                            e
                        });
                        inner
                    }
                },
                None => value,
            };
            array.push(next_val);
            if i.is_negative() {
                // We need to push to the front of the array.
                array.rotate_right(1);
            }
            retval
        }
    }

    fn insert_map(
        name: &str,
        requires_quoting: bool,
        mut working_lookup: LookupBuf,
        map: &mut BTreeMap<String, Value>,
        value: Value,
    ) -> std::result::Result<Option<Value>, ValueError> {
        let next_segment = match working_lookup.get(0) {
            Some(segment) => segment,
            None => {
                return Ok(map.insert(name.to_string(), value));
            }
        };

        map.entry(name.to_string())
            .and_modify(|entry| Value::correct_type(entry, next_segment))
            .or_insert_with(|| {
                // The entry this segment is referring to doesn't exist, so we must push the appropriate type
                // into the value.
                match next_segment {
                    SegmentBuf::Index(next_len) => {
                        Value::Array(Vec::with_capacity(next_len.abs() as usize))
                    }
                    SegmentBuf::Field(_) | SegmentBuf::Coalesce(_) => {
                        Value::Map(Default::default())
                    }
                }
            })
            .insert(working_lookup, value)
            .map_err(|mut e| {
                if let ValueError::PrimitiveDescent {
                    original_target,
                    primitive_at,
                    original_value: _,
                } = &mut e
                {
                    let segment = SegmentBuf::Field(FieldBuf {
                        name: name.to_string(),
                        requires_quoting,
                    });
                    original_target.push_front(segment.clone());
                    primitive_at.push_front(segment);
                };
                e
            })
    }

    fn insert_coalesce(
        sub_segments: Vec<FieldBuf>,
        working_lookup: &LookupBuf,
        sub_value: &mut Value,
        value: Value,
    ) -> std::result::Result<Option<Value>, ValueError> {
        // Creating a needle with a back out of the loop is very important.
        let mut needle = None;
        for sub_segment in sub_segments {
            let mut lookup = LookupBuf::from(sub_segment);
            lookup.extend(working_lookup.clone()); // We need to include the rest of the insert.
                                                   // Notice we cannot take multiple mutable borrows in a loop, so we must pay the
                                                   // contains cost extra. It's super unfortunate, hopefully future work can solve this.
            if !sub_value.contains(&lookup) {
                needle = Some(lookup);
                break;
            }
        }
        match needle {
            Some(needle) => sub_value.insert(needle, value),
            None => Ok(None),
        }
    }

    /// Ensures the value is the correct type for the given segment.
    /// An Index needs the value to be an Array, the others need it to be a Map.
    fn correct_type(value: &mut Value, segment: &SegmentBuf) {
        match segment {
            SegmentBuf::Index(next_len) if !matches!(value, Value::Array(_)) => {
                *value = Value::Array(Vec::with_capacity(next_len.abs() as usize));
            }
            SegmentBuf::Field(_) if !matches!(value, Value::Map(_)) => {
                *value = Value::Map(Default::default());
            }
            SegmentBuf::Coalesce(_set) if !matches!(value, Value::Map(_)) => {
                *value = Value::Map(Default::default());
            }
            _ => (),
        }
    }
}

fn timestamp_to_string(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

impl PartialEq<Value> for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Array(a), Value::Array(b)) => a.eq(b),
            (Value::Boolean(a), Value::Boolean(b)) => a.eq(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.eq(b),
            (Value::Float(a), Value::Float(b)) => {
                //TODO: why is this so odd, and doesn't match VRL Value

                // This compares floats with the following rules:
                // * NaNs compare as equal
                // * Positive and negative infinity are not equal
                // * -0 and +0 are not equal
                // * Floats will compare using truncated portion
                if a.is_sign_negative() == b.is_sign_negative() {
                    if a.is_finite() && b.is_finite() {
                        a.trunc().eq(&b.trunc())
                    } else {
                        a.is_finite() == b.is_finite()
                    }
                } else {
                    false
                }
            }
            (Value::Integer(a), Value::Integer(b)) => a.eq(b),
            (Value::Map(a), Value::Map(b)) => a.eq(b),
            (Value::Null, Value::Null) => true,
            (Value::Timestamp(a), Value::Timestamp(b)) => a.eq(b),
            _ => false,
        }
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self {
            Value::Bytes(x) => serializer.serialize_str(String::from_utf8_lossy(x).as_ref()),
            Value::Regex(regex) => serializer.serialize_str(&regex.to_string()),
            Value::Timestamp(timestamp) => {
                serializer.serialize_str(&timestamp_to_string(timestamp))
            }
            Value::Integer(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(f.into_inner()),
            Value::Boolean(b) => serializer.serialize_bool(*b),
            Value::Map(m) => serializer.collect_map(m),
            Value::Array(a) => serializer.collect_seq(a),
            // Regex(v) => serializer.serialize_str(&v.to_string()),
            Value::Null => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("any valid JSON value")
            }

            #[inline]
            fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
                Ok(value.into())
            }

            #[inline]
            fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
                Ok(value.into())
            }

            #[inline]
            fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
                Ok((value as i64).into())
            }

            #[inline]
            fn visit_f64<E>(self, value: f64) -> Result<Value, E>
            where
                E: serde::de::Error,
            {
                Value::try_from(value).map_err(|_| {
                    serde::de::Error::invalid_value(serde::de::Unexpected::Float(value), &self)
                })
            }

            #[inline]
            fn visit_str<E>(self, value: &str) -> Result<Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Value::Bytes(Bytes::copy_from_slice(value.as_bytes())))
            }

            #[inline]
            fn visit_string<E>(self, value: String) -> Result<Value, E> {
                Ok(Value::Bytes(value.into()))
            }

            #[inline]
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            #[inline]
            fn visit_some<D>(self, deserializer: D) -> Result<Value, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Deserialize::deserialize(deserializer)
            }

            #[inline]
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            #[inline]
            fn visit_seq<V>(self, mut visitor: V) -> Result<Value, V::Error>
            where
                V: SeqAccess<'de>,
            {
                let mut vec = Vec::new();
                while let Some(value) = visitor.next_element()? {
                    vec.push(value);
                }

                Ok(Value::Array(vec))
            }

            fn visit_map<V>(self, mut visitor: V) -> Result<Value, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut map = BTreeMap::new();
                while let Some((key, value)) = visitor.next_entry()? {
                    map.insert(key, value);
                }

                Ok(Value::Map(map))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}

/*
// VECTOR
#[derive(PartialOrd, Debug, Clone, Deserialize)]
pub enum Value {
    Bytes(Bytes),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Timestamp(DateTime<Utc>),

    Map(BTreeMap<String, Value>),
    Array(Vec<Value>),
    Null,
}

// VRL
#[derive(Debug, Clone, Hash, PartialEq)]
pub enum Value {
    Bytes(Bytes),
    Integer(i64),
    Float(NotNan<f64>),
    Boolean(bool),
    Timestamp(DateTime<Utc>),
    Object(BTreeMap<String, Value>),
    Array(Vec<Value>),

    Regex(Regex),
    Null,
}
*/

/*






// #[cfg(feature = "vrl")]
// impl From<vrl_core::Value> for Value {
//     fn from(v: vrl_core::Value) -> Self {
//         use vrl_core::Value::{
//             Array, Boolean, Bytes, Float, Integer, Null, Object, Regex, Timestamp,
//         };
//
//         match v {
//             Bytes(v) => Value::Bytes(v),
//             Integer(v) => Value::Integer(v),
//             Float(v) => Value::Float(*v),
//             Boolean(v) => Value::Boolean(v),
//             Object(v) => Value::Map(v.into_iter().map(|(k, v)| (k, v.into())).collect()),
//             Array(v) => Value::Array(v.into_iter().map(Into::into).collect()),
//             Timestamp(v) => Value::Timestamp(v),
//             Regex(v) => Value::Bytes(bytes::Bytes::copy_from_slice(v.to_string().as_bytes())),
//             Null => Value::Null,
//         }
//     }
// }
//
// #[cfg(feature = "vrl")]
// impl From<Value> for vrl_core::Value {
//     fn from(v: Value) -> Self {
//         use vrl_core::Value::{Array, Object};
//
//         match v {
//             Value::Bytes(v) => v.into(),
//             Value::Integer(v) => v.into(),
//             Value::Float(v) => v.into(),
//             Value::Boolean(v) => v.into(),
//             Value::Map(v) => Object(v.into_iter().map(|(k, v)| (k, v.into())).collect()),
//             Value::Array(v) => Array(v.into_iter().map(Into::into).collect()),
//             Value::Timestamp(v) => v.into(),
//             Value::Null => ().into(),
//         }
//     }
// }

 */

// #[cfg(test)]
// mod test {
//     use std::{fs, io::Read, path::Path};
//
//     use quickcheck::{QuickCheck, TestResult};
//
//     use super::*;
//
//     fn parse_artifact(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
//         let mut test_file = match fs::File::open(path) {
//             Ok(file) => file,
//             Err(e) => return Err(e),
//         };
//
//         let mut buf = Vec::new();
//         test_file.read_to_end(&mut buf)?;
//
//         Ok(buf)
//     }
//
//     mod value_compare {
//         use super::*;
//
//         #[test]
//         fn compare_correctly() {
//             assert!(Value::Integer(0).eq(&Value::Integer(0)));
//             assert!(!Value::Integer(0).eq(&Value::Integer(1)));
//             assert!(!Value::Boolean(true).eq(&Value::Integer(2)));
//             assert!(Value::Float(1.2).eq(&Value::Float(1.4)));
//             assert!(!Value::Float(1.2).eq(&Value::Float(-1.2)));
//             assert!(!Value::Float(-0.0).eq(&Value::Float(0.0)));
//             assert!(!Value::Float(f64::NEG_INFINITY).eq(&Value::Float(f64::INFINITY)));
//             assert!(Value::Array(vec![Value::Integer(0), Value::Boolean(true)])
//                 .eq(&Value::Array(vec![Value::Integer(0), Value::Boolean(true)])));
//             assert!(!Value::Array(vec![Value::Integer(0), Value::Boolean(true)])
//                 .eq(&Value::Array(vec![Value::Integer(1), Value::Boolean(true)])));
//         }
//     }
//
//     mod value_hash {
//         use super::*;
//
//         fn hash(a: &Value) -> u64 {
//             let mut h = std::collections::hash_map::DefaultHasher::new();
//
//             a.hash(&mut h);
//             h.finish()
//         }
//
//         #[test]
//         fn hash_correctly() {
//             assert_eq!(hash(&Value::Integer(0)), hash(&Value::Integer(0)));
//             assert_ne!(hash(&Value::Integer(0)), hash(&Value::Integer(1)));
//             assert_ne!(hash(&Value::Boolean(true)), hash(&Value::Integer(2)));
//             assert_eq!(hash(&Value::Float(1.2)), hash(&Value::Float(1.4)));
//             assert_ne!(hash(&Value::Float(1.2)), hash(&Value::Float(-1.2)));
//             assert_ne!(hash(&Value::Float(-0.0)), hash(&Value::Float(0.0)));
//             assert_ne!(
//                 hash(&Value::Float(f64::NEG_INFINITY)),
//                 hash(&Value::Float(f64::INFINITY))
//             );
//             assert_eq!(
//                 hash(&Value::Array(vec![Value::Integer(0), Value::Boolean(true)])),
//                 hash(&Value::Array(vec![Value::Integer(0), Value::Boolean(true)]))
//             );
//             assert_ne!(
//                 hash(&Value::Array(vec![Value::Integer(0), Value::Boolean(true)])),
//                 hash(&Value::Array(vec![Value::Integer(1), Value::Boolean(true)]))
//             );
//         }
//     }
//
//     mod insert_get_remove {
//         use super::*;
//
//         #[test]
//         fn single_field() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_map().unwrap()[key], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn nested_field() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root.doot";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(
//                 value.as_map().unwrap()["root"].as_map().unwrap()["doot"],
//                 marker
//             );
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn double_nested_field() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root.doot.toot";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(
//                 value.as_map().unwrap()["root"].as_map().unwrap()["doot"]
//                     .as_map()
//                     .unwrap()["toot"],
//                 marker
//             );
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn single_index() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_array()[0], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn negative_index() {
//             let mut value = Value::from(vec![Value::from(1), Value::from(2), Value::from(3)]);
//             let key = "[-2]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//
//             assert_eq!(
//                 value.insert(lookup.clone(), marker.clone()).unwrap(),
//                 Some(Value::from(2))
//             );
//             assert_eq!(value.as_array().len(), 3);
//             assert_eq!(value.as_array()[0], Value::from(1));
//             assert_eq!(value.as_array()[1], marker);
//             assert_eq!(value.as_array()[2], Value::from(3));
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//
//             let lookup = Lookup::from_str(key).unwrap();
//             assert_eq!(value.remove(lookup, true).unwrap(), Some(marker));
//             assert_eq!(value.as_array().len(), 2);
//             assert_eq!(value.as_array()[0], Value::from(1));
//             assert_eq!(value.as_array()[1], Value::from(3));
//         }
//
//         #[test]
//         fn negative_index_resize() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[-3]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_array().len(), 3);
//             assert_eq!(value.as_array()[0], marker);
//             assert_eq!(value.as_array()[1], Value::Null);
//             assert_eq!(value.as_array()[2], Value::Null);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//         }
//
//         #[test]
//         fn nested_index() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0][0]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_array()[0].as_array()[0], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn field_index() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root[0]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_map().unwrap()["root"].as_array()[0], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn field_negative_index() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root[-1]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None,);
//             assert_eq!(value.as_map().unwrap()["root"].as_array()[0], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.remove(&lookup, true).unwrap(), Some(marker),);
//             assert_eq!(value, Value::from(BTreeMap::default()),);
//         }
//
//         #[test]
//         fn index_field() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0].boot";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(value.as_array()[0].as_map().unwrap()["boot"], marker);
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn nested_index_field() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0][0].boot";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(
//                 value.as_array()[0].as_array()[0].as_map().unwrap()["boot"],
//                 marker
//             );
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn nested_index_negative() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "field[0][-1]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(
//                 value.as_map().unwrap()["field"].as_array()[0].as_array()[0],
//                 marker
//             );
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker),);
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker),);
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker),);
//         }
//
//         #[test]
//         fn field_with_nested_index_field() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "root[0][0].boot";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let mut marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             assert_eq!(
//                 value.as_map().unwrap()["root"].as_array()[0].as_array()[0]
//                     .as_map()
//                     .unwrap()["boot"],
//                 marker
//             );
//             assert_eq!(value.get(&lookup).unwrap(), Some(&marker));
//             assert_eq!(value.get_mut(&lookup).unwrap(), Some(&mut marker));
//             assert_eq!(value.remove(&lookup, false).unwrap(), Some(marker));
//         }
//
//         #[test]
//         fn populated_field() {
//             let mut value = Value::from(BTreeMap::default());
//             let marker = Value::from(true);
//             let lookup = LookupBuf::from_str("a[2]").unwrap();
//             assert_eq!(value.insert(lookup, marker.clone()).unwrap(), None);
//
//             let lookup = LookupBuf::from_str("a[0]").unwrap();
//             assert_eq!(
//                 value.insert(lookup, marker.clone()).unwrap(),
//                 Some(Value::Null)
//             );
//
//             assert_eq!(value.as_map().unwrap()["a"].as_array().len(), 3);
//             assert_eq!(value.as_map().unwrap()["a"].as_array()[0], marker);
//             assert_eq!(value.as_map().unwrap()["a"].as_array()[1], Value::Null);
//             assert_eq!(value.as_map().unwrap()["a"].as_array()[2], marker);
//
//             // Replace the value at 0.
//             let lookup = LookupBuf::from_str("a[0]").unwrap();
//             let marker = Value::from(false);
//             assert_eq!(
//                 value.insert(lookup, marker.clone()).unwrap(),
//                 Some(Value::from(true))
//             );
//             assert_eq!(value.as_map().unwrap()["a"].as_array()[0], marker);
//         }
//     }
//
//     mod corner_cases {
//         use super::*;
//
//         #[test]
//         fn remove_prune_map_with_map() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "foo.bar";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             // Since the `foo` map is now empty, this should get cleaned.
//             assert_eq!(value.remove(&lookup, true).unwrap(), Some(marker));
//             assert!(!value.contains("foo"));
//         }
//
//         #[test]
//         fn remove_prune_map_with_array() {
//             let mut value = Value::from(BTreeMap::default());
//             let key = "foo[0]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             // Since the `foo` map is now empty, this should get cleaned.
//             assert_eq!(value.remove(&lookup, true).unwrap(), Some(marker));
//             assert!(!value.contains("foo"));
//         }
//
//         #[test]
//         fn remove_prune_array_with_map() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0].bar";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             // Since the `foo` map is now empty, this should get cleaned.
//             assert_eq!(value.remove(&lookup, true).unwrap(), Some(marker));
//             assert!(!value.contains(0));
//         }
//
//         #[test]
//         fn remove_prune_array_with_array() {
//             let mut value = Value::from(Vec::<Value>::default());
//             let key = "[0][0]";
//             let lookup = LookupBuf::from_str(key).unwrap();
//             let marker = Value::from(true);
//             assert_eq!(value.insert(lookup.clone(), marker.clone()).unwrap(), None);
//             // Since the `foo` map is now empty, this should get cleaned.
//             assert_eq!(value.remove(&lookup, true).unwrap(), Some(marker));
//             assert!(!value.contains(0));
//         }
//     }
//
//     #[test]
//     fn quickcheck_value() {
//         fn inner(mut path: LookupBuf) -> TestResult {
//             let mut value = Value::from(BTreeMap::default());
//             let mut marker = Value::from(true);
//
//             if matches!(path.get(0), Some(SegmentBuf::Index(_))) {
//                 // Push a field at the start of the path since the top level is always a map.
//                 path.push_front(SegmentBuf::from("field"));
//             }
//
//             assert_eq!(
//                 value.insert(path.clone(), marker.clone()).unwrap(),
//                 None,
//                 "inserting value"
//             );
//             assert_eq!(value.get(&path).unwrap(), Some(&marker), "retrieving value");
//             assert_eq!(
//                 value.get_mut(&path).unwrap(),
//                 Some(&mut marker),
//                 "retrieving mutable value"
//             );
//
//             assert_eq!(
//                 value.remove(&path, true).unwrap(),
//                 Some(marker),
//                 "removing value"
//             );
//
//             TestResult::passed()
//         }
//
//         QuickCheck::new()
//             .tests(100)
//             .max_tests(200)
//             .quickcheck(inner as fn(LookupBuf) -> TestResult);
//     }
//
//     // This test iterates over the `tests/data/fixtures/value` folder and:
//     //   * Ensures the parsed folder name matches the parsed type of the `Value`.
//     //   * Ensures the `serde_json::Value` to `vector::Value` conversions are harmless. (Think UTF-8 errors)
//     //
//     // Basically: This test makes sure we aren't mutilating any content users might be sending.
//     #[test]
//     fn json_value_to_vector_value_to_json_value() {
//         const FIXTURE_ROOT: &str = "tests/data/fixtures/value";
//
//         std::fs::read_dir(FIXTURE_ROOT)
//             .unwrap()
//             .for_each(|type_dir| match type_dir {
//                 Ok(type_name) => {
//                     let path = type_name.path();
//                     std::fs::read_dir(path)
//                         .unwrap()
//                         .for_each(|fixture_file| match fixture_file {
//                             Ok(fixture_file) => {
//                                 let path = fixture_file.path();
//                                 let buf = parse_artifact(&path).unwrap();
//
//                                 let serde_value: serde_json::Value =
//                                     serde_json::from_slice(&*buf).unwrap();
//                                 let vector_value = Value::from(serde_value);
//
//                                 // Validate type
//                                 let expected_type = type_name
//                                     .path()
//                                     .file_name()
//                                     .unwrap()
//                                     .to_string_lossy()
//                                     .to_string();
//                                 let is_match = match vector_value {
//                                     Value::Boolean(_) => expected_type.eq("boolean"),
//                                     Value::Integer(_) => expected_type.eq("integer"),
//                                     Value::Bytes(_) => expected_type.eq("bytes"),
//                                     Value::Array { .. } => expected_type.eq("array"),
//                                     Value::Map(_) => expected_type.eq("map"),
//                                     Value::Null => expected_type.eq("null"),
//                                     _ => unreachable!("You need to add a new type handler here."),
//                                 };
//                                 assert!(
//                                     is_match,
//                                     "Typecheck failure. Wanted {}, got {:?}.",
//                                     expected_type, vector_value
//                                 );
//                                 let _value: serde_json::Value = vector_value.try_into().unwrap();
//                             }
//                             _ => panic!("This test should never read Err'ing test fixtures."),
//                         });
//                 }
//                 _ => panic!("This test should never read Err'ing type folders."),
//             });
//     }
// }

// VRL TESTS
// #[cfg(test)]
// mod test {
//     use bytes::Bytes;
//     use chrono::DateTime;
//     use indoc::indoc;
//     use ordered_float::NotNan;
//     use regex::Regex;
//     use shared::btreemap;
//
//     use super::Value;
//
//     #[test]
//     fn test_display_string() {
//         assert_eq!(
//             Value::Bytes(Bytes::from("Hello, world!")).to_string(),
//             r#""Hello, world!""#
//         );
//     }
//
//     #[test]
//     fn test_display_string_with_backslashes() {
//         assert_eq!(
//             Value::Bytes(Bytes::from(r#"foo \ bar \ baz"#)).to_string(),
//             r#""foo \\ bar \\ baz""#
//         );
//     }
//
//     #[test]
//     fn test_display_string_with_quotes() {
//         assert_eq!(
//             Value::Bytes(Bytes::from(r#""Hello, world!""#)).to_string(),
//             r#""\"Hello, world!\"""#
//         );
//     }
//
//     #[test]
//     fn test_display_string_with_newlines() {
//         assert_eq!(
//             Value::Bytes(Bytes::from(indoc! {"
//                 Some
//                 new
//                 lines
//             "}))
//                 .to_string(),
//             r#""Some\nnew\nlines\n""#
//         );
//     }
//
//     #[test]
//     fn test_display_integer() {
//         assert_eq!(Value::Integer(123).to_string(), "123");
//     }
//
//     #[test]
//     fn test_display_float() {
//         assert_eq!(
//             Value::Float(NotNan::new(123.45).unwrap()).to_string(),
//             "123.45"
//         );
//     }
//
//     #[test]
//     fn test_display_boolean() {
//         assert_eq!(Value::Boolean(true).to_string(), "true");
//     }
//
//     #[test]
//     fn test_display_object() {
//         assert_eq!(
//            Value::Map(btreemap! {
//                 "foo" => "bar"
//             })
//                 .to_string(),
//             r#"{ "foo": "bar" }"#
//         );
//     }
//
//     #[test]
//     fn test_display_array() {
//         assert_eq!(
//             Value::Array(
//                 vec!["foo", "bar"]
//                     .into_iter()
//                     .map(std::convert::Into::into)
//                     .collect()
//             )
//                 .to_string(),
//             r#"["foo", "bar"]"#
//         );
//     }
//
//     #[test]
//     fn test_display_timestamp() {
//         assert_eq!(
//             Value::Timestamp(
//                 DateTime::parse_from_rfc3339("2000-10-10T20:55:36Z")
//                     .unwrap()
//                     .into()
//             )
//                 .to_string(),
//             "t'2000-10-10T20:55:36Z'"
//         );
//     }
//
//     #[test]
//     fn test_display_regex() {
//         assert_eq!(
//             Value::Regex(Regex::new(".*").unwrap().into()).to_string(),
//             "r'.*'"
//         );
//     }
//
//     #[test]
//     fn test_display_null() {
//         assert_eq!(Value::Null.to_string(), "null");
//     }
// }
