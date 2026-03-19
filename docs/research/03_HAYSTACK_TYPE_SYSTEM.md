# 03: Haystack Type System -- C++ to libhaystack (Rust) Migration

## Overview

Sandstar's C++ codebase contains a full custom implementation of the Project Haystack type system: 30+ files totaling ~6,000 lines of value types, plus ~1,500 lines of Zinc I/O (reader, writer, tokenizer). Every one of these lines is replaced by a single dependency:

```toml
[dependencies]
libhaystack = "2.0"
```

**libhaystack** (j2inn, v2.0.4, BSD-3-Clause) is a Rust implementation of the Haystack 4 specification covering types, filter, units, and encoding/decoding. It is maintained by J2 Innovations (a Siemens company) -- the same organization that authored the original C++ port used in Sandstar.

**Source files replaced:**

| Category | Files | C++ Lines | Rust Lines |
|----------|-------|-----------|------------|
| Value types (val, dict, grid, ref, num, str, bool, date, time, datetime, coord, uri, marker, bin, na, xstr, col, row, list, hisitem, timezone, datetimerange, gridval, dict_type) | 48 (.cpp + .hpp) | ~4,900 | 0 |
| Filter system (filter.cpp/hpp) | 2 | ~700 | 0 |
| Zinc I/O (zincreader, zincwriter, tokenizer, token, gridreader, gridwriter) | 10 | ~1,870 | 0 |
| Support (proj, gridval, dict_type) | 6 | ~530 | 0 |
| **Total** | **66** | **~8,000** | **0** |

**Zero custom type code needed.** The entire Haystack type layer becomes an `extern crate` import.

---

## 2. The C++ Type Hierarchy (Current)

The existing C++ implementation follows a classical OOP inheritance hierarchy rooted at `haystack::Val`:

```
haystack::Val (abstract base, boost::noncopyable)
  |-- EmptyVal        (singleton empty/null)
  |-- Bool            (true/false)
  |-- Num             (double + unit string)
  |-- Str             (string wrapper)
  |-- Ref             (id string + optional display)
  |-- Uri             (URI string)
  |-- Marker          (singleton tag presence)
  |-- Na              (singleton Not Available)
  |-- Bin             (MIME type string)
  |-- Coord           (lat/lng in micro-degrees)
  |-- Date            (year, month, day)
  |-- Time            (hour, min, sec, ms)
  |-- DateTime         (Date + Time + TimeZone + offset)
  |-- XStr            (type string + value string)
  |-- List            (ptr_vector<Val>)
  |-- DictType        (vector<string> of marker names)
  |-- GridVal         (wraps Grid as a Val)

haystack::Dict        (ptr_map<string, Val>, separate from Val hierarchy)
  |-- Row             (Dict subclass, references Grid cells)

haystack::Grid        (cols + rows, noncopyable)
  |-- GridView        (non-owning view of Grid)

haystack::Col         (index + name + meta Dict)

haystack::Filter      (abstract, shared_ptr-based tree)
  |-- PathFilter      (has path resolution)
  |    |-- Has        (tag present)
  |    |-- Missing    (tag absent)
  |    |-- CmpFilter  (comparison base)
  |         |-- Eq, Ne, Lt, Le, Gt, Ge
  |-- CompoundFilter
       |-- And
       |-- Or

haystack::TimeZone    (name + offset)
haystack::DateTimeRange (start DateTime + end DateTime)
haystack::HisItem     (timestamp + value pair)
haystack::Path        (tag path with -> separator)
  |-- Path1           (single name)
  |-- PathN           (multiple names)
```

### Key Problems with the C++ Implementation

1. **Uses `std::auto_ptr`** -- deprecated in C++11, removed in C++17. Every type uses `auto_ptr_t` typedefs
2. **Manual clone everywhere** -- `virtual auto_ptr_t clone() const = 0` on every type, with `new_clone()` free function
3. **Null pointer comparisons** -- `val.cpp:46` compares `&other == NULL`, undefined behavior in modern C++
4. **Mutex per Dict** -- `mutable std::mutex m_mutex` in Dict protects a `boost::ptr_map`, adding overhead to every tag access
5. **Corrupted map guards** -- Dict operations have `max_iterations` guards suggesting past infinite loop bugs
6. **Incomplete implementations** -- `DictType::operator==(const Val&)` returns `false` with a TODO comment
7. **Memory ownership confusion** -- `Dict::add(const Val*)` takes raw pointer ownership, `Dict::add(const Val&)` clones; easy to mix up

---

## 3. The libhaystack Rust Equivalent

### 3.1 Value Enum (replaces Val hierarchy + EmptyVal)

**C++ current** (`val.hpp`, 102 lines):
```cpp
namespace haystack {
class Val : boost::noncopyable {
public:
    enum Type {
        BOOL_TYPE = 'B', BIN_TYPE = 'b', COORD_TYPE = 'C',
        DATE_TIME_TYPE = 'd', DATE_TYPE = 'D', GRID_TYPE = 'G',
        MARKER_TYPE = 'M', NUM_TYPE = 'N', REF_TYPE = 'R',
        STR_TYPE = 'S', TIME_TYPE = 'T', URI_TYPE = 'U',
        XSTR_TYPE = 'X', NA_TYPE = 'n', LIST_TYPE = 'L',
        DICT_TYPE = 'A', EMPTY_TYPE = '|'
    };
    typedef std::auto_ptr<Val> auto_ptr_t;
    virtual const std::string to_zinc() const = 0;
    virtual const Type type() const = 0;
    virtual bool operator==(const Val &other) const = 0;
    virtual bool operator>(const Val &other) const = 0;
    virtual bool operator<(const Val &other) const = 0;
    template <class ValType> inline const ValType &as() const;
    virtual auto_ptr_t clone() const = 0;
    const bool is_empty() const;
};
}
```

**Rust replacement** (`libhaystack::haystack::val::Value`):
```rust
use libhaystack::val::*;

// Value is an algebraic type (enum) -- 18 variants
pub enum Value {
    Null,           // replaces EmptyVal
    Marker,         // replaces Marker singleton
    Remove,         // Haystack 4 addition (no C++ equivalent)
    Na,             // replaces Na singleton
    Bool(Bool),     // replaces Bool class
    Number(Number), // replaces Num class
    Str(Str),       // replaces Str class
    Ref(Ref),       // replaces Ref class
    Symbol(Symbol), // Haystack 4 addition (no C++ equivalent)
    Uri(Uri),       // replaces Uri class
    Date(Date),     // replaces Date class
    Time(Time),     // replaces Time class
    DateTime(DateTime), // replaces DateTime class
    Coord(Coord),   // replaces Coord class
    XStr(XStr),     // replaces XStr class (and Bin)
    List(List),     // replaces List class
    Dict(Dict),     // replaces Dict class
    Grid(Grid),     // replaces GridVal wrapper
}
```

**Usage comparison:**

```cpp
// C++ -- creating a value, type checking, casting
Val::auto_ptr_t val(new Num(72.5, "°F"));
if (val->type() == Val::NUM_TYPE) {
    double v = val->as<Num>().value;
    std::string u = val->as<Num>().unit;
}
std::string zinc = val->to_zinc();  // "72.5°F"
```

```rust
// Rust -- creating a value, pattern matching
let val = Value::make_number_unit(72.5, get_unit("°F"));
if let Value::Number(num) = &val {
    let v: f64 = num.value;
    let u: &Option<Unit> = &num.unit;
}
// Or use the zinc encoder
use libhaystack::encoding::zinc;
let zinc_str = zinc::encode::to_string(&val);  // "72.5°F"
```

**Rust improvements:**
- No heap allocation for simple values (`Marker`, `Na`, `Bool`, `Null` are zero-cost enum variants)
- Pattern matching replaces unsafe `static_cast` / C-style casts
- `Clone`, `PartialEq`, `Debug`, `Display` derived automatically
- No `auto_ptr` or manual memory management
- `Remove` and `Symbol` variants support Haystack 4 protocol (C++ only had Haystack 3)

---

### 3.2 Bool (replaces `bool.cpp/hpp`, 125 lines)

**C++ current** (`bool.hpp`, 55 lines):
```cpp
class Bool : public Val {
public:
    const Type type() const { return BOOL_TYPE; }
    const bool value;
    Bool(bool val);
    static const Bool &TRUE_VAL;    // singleton
    static const Bool &FALSE_VAL;   // singleton
    const std::string to_zinc() const;  // "T" or "F"
    bool operator==(const Bool &other) const;
    bool operator==(const Val &other) const;
    auto_ptr_t clone() const;
};
```

**Rust replacement:**
```rust
// No dedicated Bool struct needed -- it is a variant of Value
let val = Value::from(true);       // Value::Bool(Bool::from(true))
let val = Value::from(false);      // Value::Bool(Bool::from(false))

// Type checking
assert!(val.is_bool());

// Extract
if let Value::Bool(b) = &val {
    let native: bool = b.value;
}
```

---

### 3.3 Num (replaces `num.cpp/hpp`, 231 lines)

**C++ current** (`num.hpp`, 95 lines):
```cpp
class Num : public Val {
public:
    const Type type() const { return NUM_TYPE; }
    const double value;
    const std::string unit;
    Num(double val, const std::string &unit);
    Num(double val);
    Num(int val, const std::string &unit);
    Num(long long val);
    static const Num ZERO;
    static const Num POS_INF;
    static const Num NEG_INF;
    static const Num NaN;
    const std::string to_zinc() const;
    bool operator==(const Num &other) const;
    static bool is_unit_name(const std::string &);
};
```

**Rust replacement** (`libhaystack::haystack::val::Number`):
```rust
use libhaystack::val::*;
use libhaystack::units::*;

// Number with unit
let temp = Value::make_number_unit(72.5, get_unit("°F"));
let raw = Value::make_number(4095.0);  // no unit

// Access
if let Value::Number(num) = &temp {
    let v: f64 = num.value;
    let u: &Option<Unit> = &num.unit;
}

// Special values
let inf = Value::make_number(f64::INFINITY);
let nan = Value::make_number(f64::NAN);
```

**Rust improvements:**
- Unit is a strongly-typed `Unit` struct (not a bare string) backed by the full Project Haystack unit database
- `get_unit("°F")` validates the unit name at runtime with the Haystack unit ontology
- No custom `is_unit_name()` validation needed
- `PartialOrd` trait provides comparison operators automatically

---

### 3.4 Str (replaces `str.cpp/hpp`, 190 lines)

**C++ current** (`str.hpp`, 68 lines):
```cpp
class Str : public Val {
public:
    const Type type() const { return STR_TYPE; }
    const std::string value;
    Str(const std::string &val);
    static const Str& EMPTY;
    const std::string to_zinc() const;  // handles escape sequences
    bool operator==(const Str &other) const;
    bool operator==(const std::string &other) const;
    auto_ptr_t clone() const;
};
```

**Rust replacement:**
```rust
use libhaystack::val::*;

let val = Value::from("Hello, World!");  // Value::Str(Str::from("Hello, World!"))
let val = Value::make_str("sensor-001");

// Extract
if let Value::Str(s) = &val {
    let native: &str = s.value.as_str();
}
```

**Rust improvement:** Zinc encoding of special characters (newlines, quotes, backslashes, unicode escapes) is handled by `libhaystack::encoding::zinc::encode` -- the 40-line escape loop in `str.cpp:38-72` is eliminated.

---

### 3.5 Ref (replaces `ref.cpp/hpp`, 188 lines)

**C++ current** (`ref.hpp`, 75 lines):
```cpp
class Ref : public Val {
public:
    const Type type() const { return REF_TYPE; }
    const std::string value;
    Ref(const std::string &val);
    Ref(const std::string &val, const std::string &dis);
    const std::string to_code() const;  // "@id"
    const std::string to_zinc() const;  // "@id \"display\""
    const std::string dis() const;
    bool operator==(const Ref &other) const;
    static bool is_id_char(int c);
    static bool is_id(const std::string &);
private:
    const Str m_dis;
    void enforceId();  // throws if invalid
};
```

**Rust replacement** (`libhaystack::haystack::val::Ref`):
```rust
use libhaystack::val::*;

let r = Value::make_ref("p:abc-123");
let r = Value::make_ref_dis("p:abc-123", "Main AHU");

if let Value::Ref(ref_val) = &r {
    let id: &str = ref_val.value.as_str();
    let dis: &Option<String> = &ref_val.dis;
}
```

**Rust improvement:** `Option<String>` for display name is clearer than embedding a `Str` member. ID validation uses the standard Haystack 4 rules.

---

### 3.6 Marker (replaces `marker.cpp/hpp`, 120 lines)

**C++ current** (`marker.hpp`, 52 lines):
```cpp
class Marker : public Val {
    Marker(const Marker&);  // disabled
public:
    Marker() {};
    const Type type() const { return MARKER_TYPE; }
    static const Marker& VAL;  // singleton
    const std::string to_zinc() const;  // "M"
    bool operator==(const Marker &b) const;  // always true
    auto_ptr_t clone() const;  // new Marker()
};
```

**Rust replacement:**
```rust
use libhaystack::val::Value;

let m = Value::make_marker();  // Value::Marker

// In dict construction
use libhaystack::dict;
let d = dict! {
    "site" => Value::make_marker(),
    "dis"  => Value::from("Main Building"),
};
```

**Rust improvement:** `Value::Marker` is a zero-size enum variant. No heap allocation, no singleton pattern, no disabled copy constructor.

---

### 3.7 Na (replaces `na.cpp/hpp`, 109 lines)

**C++ current:**
```cpp
class Na : public Val {
    Na(const Na&);   // disabled
public:
    Na() {};
    static const Na& NA;  // singleton
    const std::string to_zinc() const;  // "NA"
};
```

**Rust replacement:**
```rust
let na = Value::Na;  // zero-cost enum variant
```

---

### 3.8 Date (replaces `date.cpp/hpp`, 278 lines)

**C++ current** (`date.hpp`, 96 lines):
```cpp
class Date : public Val {
public:
    const Type type() const { return DATE_TYPE; }
    const int year;
    const int month;
    const int day;
    Date(const std::string &s);  // parse "YYYY-MM-DD"
    Date(int year, int month, int day);
    const std::string to_zinc() const;  // "YYYY-MM-DD"
    Date inc_days(int) const;
    Date dec_days(int) const;
    int weekday() const;
    std::auto_ptr<DateTime> midnight(const TimeZone &tz) const;
    static bool is_leap_year(int year);
    static int days_in_month(int year, int mon);
    static const Date today();
};
```

**Rust replacement** (`libhaystack::haystack::val::Date`):
```rust
use libhaystack::val::*;

let d = Date::from((2024, 6, 15));
let val = Value::from(d);

// Parse from string via zinc decoder
use libhaystack::encoding::zinc;
let val: Value = zinc::decode::from_str("2024-06-15").unwrap();
```

**Rust improvement:** Date arithmetic uses the `chrono` crate internally, which is battle-tested. The 182-line `date.cpp` (leap year logic, day-of-week, month lengths) becomes a few trait implementations.

---

### 3.9 Time (replaces `time.cpp/hpp`, 234 lines)

**C++ current** (`time.hpp`, 108 lines):
```cpp
class Time : public Val {
public:
    const Type type() const { return TIME_TYPE; }
    const int hour;
    const int minutes;
    const int sec;
    const int ms;
    Time(int hour, int minutes, int sec, int ms);
    Time(const std::string& s);  // parse "hh:mm:ss"
    const std::string to_zinc() const;  // "hh:mm:ss.FFF"
    static const Time& MIDNIGHT;
};
```

**Rust replacement** (`libhaystack::haystack::val::Time`):
```rust
use libhaystack::val::*;

let t = Time::from((14, 30, 0, 0));  // 2:30 PM
let val = Value::from(t);
```

---

### 3.10 DateTime (replaces `datetime.cpp/hpp`, 305 lines)

**C++ current** (`datetime.hpp`, 106 lines):
```cpp
class DateTime : public Val {
public:
    const Type type() const { return DATE_TIME_TYPE; }
    const Date date;
    const Time time;
    const TimeZone tz;
    const int tz_offset;
    DateTime(int year, int month, int day, int hour, int min, int sec,
             const TimeZone &tz, int tzOffset);
    static DateTime fromString(const std::string &s);
    static DateTime make_time_t(const time_t &ts, const TimeZone &);
    static DateTime make(const int64_t &time, const TimeZone &);
    static DateTime now(const TimeZone &);
    const std::string to_zinc() const;  // "YYYY-MM-DD'T'hh:mm:ss.FFFz zzzz"
    const int64_t millis() const;
};
```

**Rust replacement** (`libhaystack::haystack::val::DateTime`):
```rust
use libhaystack::val::*;

// Current time
let now = DateTime::now();
let val = Value::from(now);

// From components
let dt = DateTime::from((
    Date::from((2024, 6, 15)),
    Time::from((14, 30, 0, 0)),
    // timezone handled by chrono
));
```

**Rust improvement:** Timezone handling uses `chrono-tz` with the full IANA timezone database. The C++ `TimeZone` class (61 lines) with its manual `detect_gmt_offset()` is eliminated.

---

### 3.11 Coord (replaces `coord.cpp/hpp`, 221 lines)

**C++ current** (`coord.hpp`, 78 lines):
```cpp
class Coord : public Val {
public:
    const Type type() const { return COORD_TYPE; }
    const int32_t ulat;  // micro-degrees
    const int32_t ulng;  // micro-degrees
    Coord(double, double);
    static Coord make(const std::string &val);  // parse "C(lat,lng)"
    static bool is_lat(double lat);
    static bool is_lng(double lng);
    double lat() const;
    double lng() const;
    const std::string to_zinc() const;  // "C(lat,lng)"
};
```

**Rust replacement** (`libhaystack::haystack::val::Coord`):
```rust
use libhaystack::val::*;

let c = Coord { lat: 37.5458, lng: -77.4491 };
let val = Value::from(c);
```

---

### 3.12 Uri (replaces `uri.cpp/hpp`, 148 lines)

**C++ current** (`uri.hpp`, 62 lines):
```cpp
class Uri : public Val {
public:
    const Type type() const { return URI_TYPE; }
    const std::string value;
    Uri(const std::string &val);
    static const Uri& EMPTY;
    const std::string to_zinc() const;  // "`uri`"
};
```

**Rust replacement:**
```rust
use libhaystack::val::*;

let u = Value::make_uri("http://example.com/api");
```

---

### 3.13 Bin (replaces `bin.cpp/hpp`, 142 lines)

**C++ current** (`bin.hpp`, 55 lines):
```cpp
class Bin : public Val {
public:
    const Type type() const { return BIN_TYPE; }
    const std::string value;  // MIME type
    Bin(const std::string &val);
    const std::string to_zinc() const;  // "Bin(\"mime\")"
};
```

**Rust replacement:** In Haystack 4 (libhaystack), `Bin` is modeled as an `XStr` with type "Bin":
```rust
use libhaystack::val::*;

// Bin is now an XStr in Haystack 4
let bin = Value::make_xstr("Bin", "application/octet-stream");
```

---

### 3.14 XStr (replaces `xstr.cpp/hpp`, 209 lines)

**C++ current** (`xstr.hpp`, 72 lines):
```cpp
class XStr : public Val {
public:
    const Type type() const { return XSTR_TYPE; }
    std::string xstr_type;
    std::string value;
    XStr(const std::string type, const std::string val);
    const std::string to_zinc() const;  // Type("value")
    void *decode(std::string type, const std::string &val);
};
```

**Rust replacement** (`libhaystack::haystack::val::XStr`):
```rust
use libhaystack::val::*;

let x = Value::make_xstr("Span", "2024-06-15");
```

---

### 3.15 List (replaces `list.cpp/hpp`, 165 lines)

**C++ current** (`list.hpp`, 46 lines):
```cpp
class List : public Val {
public:
    typedef boost::ptr_vector<Val> item_t;
    const Type type() const { return LIST_TYPE; }
    item_t items;
    List(item_t);
    const std::string to_zinc() const;  // "[v1, v2, ...]"
};
```

**Rust replacement** (`libhaystack::haystack::val::List`):
```rust
use libhaystack::val::*;

let list = Value::make_list(vec![
    Value::from(1),
    Value::from(2),
    Value::from("three"),
]);
```

---

### 3.16 Dict (replaces `dict.cpp/hpp`, 560 lines)

**C++ current** (`dict.hpp`, 171 lines):
```cpp
class Dict : boost::noncopyable {
public:
    typedef std::auto_ptr<Dict> auto_ptr_t;
    typedef boost::ptr_map<std::string, haystack::Val> dict_t;
    static const Dict &EMPTY;
    const bool is_empty() const;
    const size_t size() const;
    const bool has(const std::string &name) const;
    const bool missing(const std::string &name) const;
    const Ref &id() const;
    const Val &get(const std::string &name, bool checked = true) const;
    const std::string to_zinc() const;
    const std::string dis() const;
    bool get_bool(const std::string &name) const;
    const std::string &get_str(const std::string &name) const;
    const Ref &get_ref(const std::string &name) const;
    double get_double(const std::string &name) const;
    Dict &add(const std::string& name, Val::auto_ptr_t val);
    Dict &add(const std::string& name, const Val *val);
    Dict &add(const std::string& name, const Val &val);
    Dict &add(const std::string& name);  // marker
    Dict &add(const std::string& name, const std::string &val);
    Dict &add(const std::string& name, double val, const std::string &unit = "");
    Dict &add(const Dict &other);
    Dict &erase(const std::string& tag);
    virtual auto_ptr_t clone();
private:
    dict_t m_map;
    mutable std::mutex m_mutex;  // thread safety
};
```

**Rust replacement** (`libhaystack::haystack::val::Dict`, backed by `BTreeMap<String, Value>`):
```rust
use libhaystack::val::*;
use libhaystack::dict;

// Construction with dict! macro
let point = dict! {
    "id"      => Value::make_ref("p:abc-123"),
    "dis"     => Value::from("Zone Temp"),
    "point"   => Value::make_marker(),
    "sensor"  => Value::make_marker(),
    "temp"    => Value::make_marker(),
    "kind"    => Value::from("Number"),
    "unit"    => Value::from("°F"),
    "channel" => Value::make_number(1113.0),
    "curVal"  => Value::make_number_unit(72.5, get_unit("°F")),
};

// Access
assert!(point.has("point"));
assert!(!point.has("equip"));
assert!(point.is_empty() == false);

let dis: &str = point.get_str("dis").unwrap();
let channel: f64 = point.get_double("channel").unwrap();

// Get raw Value
let val: Option<&Value> = point.get("curVal");

// Iteration
for (key, value) in point.iter() {
    println!("{}: {:?}", key, value);
}
```

**Rust improvements:**
- `BTreeMap<String, Value>` is inherently sorted (consistent iteration order)
- No mutex needed -- Rust ownership model prevents data races at compile time
- `dict!` macro provides builder-pattern construction
- No 6 overloaded `add()` methods -- `Value::from()` trait handles type conversion
- No `auto_ptr_t` / `new_clone()` dance -- `Clone` trait is standard
- `Option<&Value>` for missing tags instead of `EmptyVal::DEF` sentinel

### Method mapping: C++ Dict to Rust Dict

| C++ Method | Rust Equivalent | Notes |
|------------|----------------|-------|
| `Dict()` | `Dict::new()` | |
| `Dict::EMPTY` | `Dict::new()` or `Dict::default()` | No singleton needed |
| `is_empty()` | `.is_empty()` | Same |
| `size()` | `.len()` | Rust convention |
| `has(name)` | `.has("name")` | Same |
| `missing(name)` | `!dict.has("name")` | No dedicated method needed |
| `get(name, true)` | `.get("name").unwrap()` | Panics if missing |
| `get(name, false)` | `.get("name")` | Returns `Option<&Value>` |
| `get_bool(name)` | `.get_bool("name")` | Returns `Option<bool>` |
| `get_str(name)` | `.get_str("name")` | Returns `Option<&str>` |
| `get_ref(name)` | `.get_ref("name")` | Returns `Option<&Ref>` |
| `get_double(name)` | `.get_double("name")` | Returns `Option<f64>` |
| `id()` | `.get_ref("id")` | No special method needed |
| `dis()` | `.get_str("dis")` | No special method needed |
| `add(name, val)` | `.insert("name", value)` | Standard map insert |
| `add(name)` | `.insert("name", Value::Marker)` | Marker shorthand |
| `add(name, str)` | `.insert("name", Value::from(str))` | Type conversion |
| `add(name, double, unit)` | `.insert("name", Value::make_number_unit(...))` | |
| `erase(name)` | `.remove("name")` | Standard map remove |
| `clone()` | `.clone()` | Derived trait |
| `to_zinc()` | `zinc::encode::to_string(&Value::from(dict))` | Via encoder |
| `operator==` | `PartialEq` derived | Automatic |

---

### 3.17 Grid (replaces `grid.cpp/hpp`, 626 lines + `row.cpp/hpp`, `col.cpp/hpp`)

**C++ current** (`grid.hpp`, 243 lines):
```cpp
class Grid : boost::noncopyable {
public:
    typedef boost::ptr_vector<Row> row_vec_t;
    typedef boost::ptr_vector<Col> col_vec_t;
    bool operator==(const Grid &other) const;
    Dict &meta();
    const bool is_err() const;
    const bool is_empty() const;
    const size_t num_rows() const;
    const Row &row(size_t row) const;
    const size_t num_cols() const;
    const Col &col(size_t index) const;
    const Col *const col(const std::string &name) const;
    Dict &add_col(const std::string &name);
    Grid &add_row(Val *[], size_t count);
    void reserve_rows(size_t count);
    static Grid::auto_ptr_t make_err(const std::runtime_error &);
    static Grid::auto_ptr_t make(const Dict &);
    static Grid::auto_ptr_t make(const std::vector<const Dict *> &);
    static Grid::auto_ptr_t make(const boost::ptr_vector<Dict> &);
    Grid();
    Grid(const std::vector<std::string>& colnames);
private:
    row_vec_t m_rows;
    col_vec_t m_cols;
    name_col_map_t m_cols_by_name;
    Dict m_meta;
};
```

**Rust replacement** (`libhaystack::haystack::val::Grid`):
```rust
use libhaystack::val::*;
use libhaystack::dict;

// Create grid from vector of Dicts (most common pattern)
let row1 = dict! {
    "id"   => Value::make_ref("p:001"),
    "dis"  => Value::from("Zone Temp"),
    "point" => Value::make_marker(),
    "curVal" => Value::make_number_unit(72.5, get_unit("°F")),
};
let row2 = dict! {
    "id"   => Value::make_ref("p:002"),
    "dis"  => Value::from("Supply Fan"),
    "point" => Value::make_marker(),
    "curVal" => Value::make_number_unit(1.0, get_unit("%")),
};

let grid = Value::make_grid_from_dicts(vec![row1, row2]);

// Access grid data
if let Value::Grid(g) = &grid {
    assert_eq!(g.len(), 2);         // 2 rows
    let first_row: &Dict = &g[0];   // index access
    let dis = first_row.get_str("dis").unwrap();
}

// Error grid
let err_grid = Grid::make_err("Something went wrong");

// Filter grid rows (see Section 4 below)
use libhaystack::filter::Filter;
let filter = Filter::try_from("point and curVal > 50").unwrap();
// Apply filter to find matching rows
```

**C++ `GridView` and `Row` are eliminated** -- In Rust, a Grid is a `Vec<Dict>` with column metadata. There is no separate `Row` type; each row is simply a `Dict`. This eliminates the `Row` class (137 lines), the `const_row_iterator` machinery, and the `GridView` wrapper (80 lines).

### Method mapping: C++ Grid to Rust Grid

| C++ Method | Rust Equivalent | Notes |
|------------|----------------|-------|
| `Grid()` | `Grid::default()` | |
| `Grid::EMPTY` | `Grid::default()` | |
| `meta()` | `.meta` field | Direct access |
| `is_err()` | `.meta.has("err")` | Check meta tag |
| `is_empty()` | `.is_empty()` | Same |
| `num_rows()` | `.len()` | Rust convention |
| `row(i)` | `grid[i]` | Index trait |
| `num_cols()` | `.columns().len()` | |
| `col(name)` | `.columns().iter().find(...)` | |
| `add_col(name)` | Builder pattern | Columns inferred from dicts |
| `add_row(vals)` | `.push(dict)` | Push a Dict |
| `reserve_rows(n)` | `.reserve(n)` | Same |
| `make(Dict)` | `Grid::from(dict)` | |
| `make(vec<Dict*>)` | `Grid::from(vec![dict1, dict2])` | |
| `make_err(e)` | `Grid::make_err(msg)` | |
| `begin()/end()` | `.iter()` | Standard Rust iteration |

---

### 3.18 Col and Row (replaced by Grid internals)

**C++ current** (`col.hpp`, 60 lines; `row.hpp`, 137 lines):

The `Col` type (index + name + meta Dict) and `Row` type (Dict subclass with cell vector) are internal Grid mechanics. In libhaystack, a Grid row is simply a `Dict` and column metadata is managed internally. The ~200 lines of Col + Row code are eliminated.

---

### 3.19 HisItem (replaces `hisitem.cpp/hpp`, 105 lines)

**C++ current** (`hisitem.hpp`, 45 lines):
```cpp
class HisItem {
public:
    boost::shared_ptr<const DateTime> ts;
    boost::shared_ptr<const Val> val;
    HisItem(const DateTime &ts, const Val &val);
    static const std::vector<HisItem> grid_to_items(const Grid &grid);
    static Grid::auto_ptr_t his_items_to_grid(const Dict &meta,
                                               const std::vector<HisItem> &items);
};
```

**Rust replacement:** History items are simply Dicts with `ts` and `val` keys, assembled into a Grid:
```rust
use libhaystack::val::*;
use libhaystack::dict;

// A history item is just a Dict row in a Grid
let his_row = dict! {
    "ts"  => Value::from(DateTime::now()),
    "val" => Value::make_number_unit(72.5, get_unit("°F")),
};

// History grid
let his_grid = Value::make_grid_from_dicts(vec![his_row]);
```

No separate `HisItem` struct is needed. The `grid_to_items()` / `his_items_to_grid()` conversions become trivial Grid/Dict operations.

---

### 3.20 TimeZone (replaces `timezone.cpp/hpp`, 155 lines)

**C++ current** (`timezone.hpp`, 61 lines):
```cpp
class TimeZone {
public:
    const std::string name;
    const int offset;
    TimeZone(const std::string& name);
    TimeZone(const std::string& name, const int offset);
    static const TimeZone UTC;
    static const TimeZone DEFAULT;
private:
    int detect_gmt_offset(std::string name);
};
```

**Rust replacement:** libhaystack uses `chrono-tz` which provides the complete IANA timezone database. No custom TimeZone class needed.

```rust
// Timezone is part of DateTime in libhaystack
use chrono_tz::America::New_York;
// libhaystack handles timezone internally via DateTime
```

---

### 3.21 DateTimeRange (replaces `datetimerange.cpp/hpp`, 241 lines)

**C++ current** (`datetimerange.hpp`, 103 lines):
```cpp
class DateTimeRange : boost::noncopyable {
public:
    enum WeekDays { SUNDAY = 1, MONDAY, TUESDAY, WEDNESDAY, THURSDAY, FRIDAY, SATURDAY };
    DateTimeRange(const Date &date, const TimeZone &tz);
    DateTimeRange(const DateTime &start, const DateTime &end);
    const DateTime &start() const;
    const DateTime &end() const;
    static DateTimeRange::auto_ptr_t this_week(const TimeZone &tz);
    static DateTimeRange::auto_ptr_t this_month(const TimeZone &tz);
    static DateTimeRange::auto_ptr_t this_year(const TimeZone &tz);
    static DateTimeRange::auto_ptr_t last_week(const TimeZone &tz);
    static DateTimeRange::auto_ptr_t last_month(const TimeZone &tz);
    static DateTimeRange::auto_ptr_t make(std::string str, const TimeZone &tz);
};
```

**Rust replacement:** A simple struct with two `DateTime` fields, with convenience constructors. In Sandstar, this is primarily used for `hisRead` range parsing:

```rust
// DateTimeRange can be a simple Rust struct
pub struct DateTimeRange {
    pub start: DateTime,
    pub end: DateTime,
}

// Or use the filter/zinc system to parse range strings:
use libhaystack::encoding::zinc;
// "today", "yesterday", "2024-06-15", etc. handled by zinc decoder
```

Note: `DateTimeRange` is not a core Haystack type -- it is a utility for the `hisRead` op. In Rust, this would be a small Sandstar-specific struct (~30 lines) rather than a libhaystack type.

---

### 3.22 DictType (replaces `dict_type.cpp/hpp`, 193 lines)

**C++ current** -- a non-standard extension that stores marker names as a list of strings:
```cpp
class DictType : public Val {
public:
    typedef std::vector<std::string> item_t;
    item_t items;
    const std::string to_zinc() const;  // "{tag1 tag2 tag3}"
    // NOTE: operator==(const Val&) returns false (TODO comment)
};
```

**Rust replacement:** In Haystack 4, this concept is replaced by proper Dict values nested inside other Dicts. The `DictType` class with its incomplete implementation is eliminated entirely.

---

### 3.23 GridVal (replaces `gridval.cpp/hpp`, 104 lines)

**C++ current** -- wraps a Grid to make it a Val subclass:
```cpp
class GridVal : public Val {
public:
    const Type type() const { return GRID_TYPE; }
    Grid m_grid;
    const std::string to_zinc() const;
};
```

**Rust replacement:** In libhaystack, `Value::Grid(Grid)` is a direct enum variant. No wrapper class needed.

---

### 3.24 Proj (replaces `proj.cpp/hpp`, 165 lines)

**C++ current** (`proj.hpp`, 111 lines):
```cpp
class Proj {
public:
    virtual Dict::auto_ptr_t about() const = 0;
    virtual Dict::auto_ptr_t read_by_id(const Ref& id) const;
    virtual Grid::auto_ptr_t read_by_ids(const boost::ptr_vector<Ref>& ids) const;
    virtual Dict::auto_ptr_t read(const std::string& filter) const;
    virtual Grid::auto_ptr_t read_all(const std::string& filter) const;
    virtual Dict::auto_ptr_t on_read_by_id(const Ref& id) const = 0;
    virtual Grid::auto_ptr_t on_read_by_ids(const boost::ptr_vector<Ref>& ids) const = 0;
    virtual Grid::auto_ptr_t on_read_all(const std::string& filter, size_t limit) const = 0;
};
```

**Rust replacement:** This becomes a Rust trait:
```rust
use libhaystack::val::*;
use libhaystack::filter::Filter;

pub trait HaystackProject {
    fn about(&self) -> Dict;
    fn read_by_id(&self, id: &Ref) -> Option<Dict>;
    fn read_by_ids(&self, ids: &[Ref]) -> Grid;
    fn read(&self, filter: &str) -> Option<Dict>;
    fn read_all(&self, filter: &str, limit: usize) -> Grid;
}
```

This is Sandstar-specific application logic, not a type system component. It moves to the REST API layer (see `04_REST_API_AXUM_MIGRATION.md`).

---

## 4. Filter System (replaces `filter.cpp/hpp`, ~700 lines)

### C++ current

The C++ filter system is a 13-class hierarchy:

```cpp
// Parsing
Filter::shared_ptr_t f = Filter::make("point and curVal > 50");

// Factories
Filter::shared_ptr_t f = Filter::has("site");
Filter::shared_ptr_t f = Filter::eq("dis", Val::auto_ptr_t(new Str("AHU-1")));
Filter::shared_ptr_t f = Filter::gt("temp", Val::auto_ptr_t(new Num(70)));

// Compound
Filter::shared_ptr_t compound = f1->AND(f2);
Filter::shared_ptr_t compound = f1->OR(f2);

// Evaluation (requires Pather interface for ref traversal)
class Pather {
public:
    virtual const Dict &find(const std::string &ref) const = 0;
};
bool match = filter->include(dict, pather);
```

### Rust replacement (`libhaystack::haystack::filter`)

```rust
use libhaystack::filter::Filter;
use libhaystack::val::*;
use libhaystack::dict;

// Parse filter from string
let filter = Filter::try_from("point and curVal > 50").expect("valid filter");

// Alternative: parse Haystack 4 ontology filters
let filter = Filter::try_from(r#"^geoPlace and dis=="Test""#).expect("filter");

// Apply to a Dict
let point = dict! {
    "point"  => Value::make_marker(),
    "curVal" => Value::make_number(72.5),
    "dis"    => Value::from("Zone Temp"),
};
assert_eq!(point.filter(&filter), true);

// Apply to Grid rows (find all matching)
if let Value::Grid(grid) = &grid_val {
    let matching: Vec<&Dict> = grid.iter()
        .filter(|row| row.filter(&filter))
        .collect();
}
```

**Rust improvements:**
- `Filter::try_from(&str)` replaces `ZincReader` + `Filter::make()` -- single entry point
- `dict.filter(&filter)` replaces `filter->include(dict, pather)` -- no Pather interface
- Haystack 4 ontology support (`^geoPlace`, `^equip`) -- C++ only supports Haystack 3 filters
- No `shared_ptr` / `enable_shared_from_this` machinery
- No 13-class hierarchy -- the filter AST is an internal enum

### Filter operator mapping

| C++ Factory | Rust Filter Syntax | Description |
|-------------|-------------------|-------------|
| `Filter::has("tag")` | `"tag"` | Has tag |
| `Filter::missing("tag")` | `"not tag"` | Missing tag |
| `Filter::eq("a", new Str("x"))` | `"a == \"x\""` | Equals |
| `Filter::ne("a", new Num(5))` | `"a != 5"` | Not equals |
| `Filter::lt("a", new Num(5))` | `"a < 5"` | Less than |
| `Filter::le("a", new Num(5))` | `"a <= 5"` | Less or equal |
| `Filter::gt("a", new Num(5))` | `"a > 5"` | Greater than |
| `Filter::ge("a", new Num(5))` | `"a >= 5"` | Greater or equal |
| `f1->AND(f2)` | `"expr1 and expr2"` | Logical AND |
| `f1->OR(f2)` | `"expr1 or expr2"` | Logical OR |

All filters are parsed from strings using `Filter::try_from()`. No programmatic filter construction needed (though the internal API supports it).

---

## 5. Zinc I/O (replaces io/ directory, ~1,870 lines)

### C++ current

```
io/
  zincreader.cpp  (883 lines) -- hand-written recursive descent parser
  zincreader.hpp  (114 lines) -- ZincReader class
  zincwriter.cpp  (131 lines) -- Grid-to-Zinc serializer
  zincwriter.hpp  (55 lines)  -- ZincWriter class
  tokenizer.cpp   (428 lines) -- character-level tokenizer
  tokenizer.hpp   (69 lines)  -- token stream
  token.cpp       (45 lines)  -- token types
  token.hpp       (91 lines)  -- token enum
  gridreader.hpp  (27 lines)  -- abstract reader
  gridwriter.hpp  (27 lines)  -- abstract writer
```

Key C++ API:
```cpp
// Read
auto reader = ZincReader::make(zinc_string);
auto grid = reader->read_grid();
auto dict = reader->read_dict();
auto val  = reader->read_scalar();
auto filter = reader->read_filter();

// Write
std::string zinc = ZincWriter::grid_to_string(grid);
```

### Rust replacement (`libhaystack::encoding::zinc`)

```rust
use libhaystack::val::*;
use libhaystack::encoding::zinc;

// Decode (replaces ZincReader, 883 lines)
let grid: Grid = zinc::decode::from_str(zinc_string).unwrap();
let val: Value = zinc::decode::from_str("72.5°F").unwrap();

// Encode (replaces ZincWriter, 131 lines)
let zinc_string: String = zinc::encode::to_string(&grid_value);

// Streaming decode for large grids (performance-oriented lazy parser)
let reader = zinc::decode::GridReader::new(zinc_bytes);
for row in reader {
    let dict: Dict = row.unwrap();
    // process row
}
```

**Rust improvements:**
- Streaming/lazy Grid parser for large datasets (C++ loads entire grid into memory)
- Standard Rust `FromStr` / `Display` traits for encoding/decoding
- Error handling via `Result` instead of C++ exceptions
- No manual character-level tokenizer -- the 428-line `tokenizer.cpp` is eliminated

---

## 6. JSON Encoding (not in C++ codebase)

The C++ Sandstar implementation only supports Zinc encoding. libhaystack adds full JSON encoding via Serde:

```rust
use libhaystack::val::*;
use libhaystack::encoding::json;

// JSON encode
let json_string: String = serde_json::to_string(&grid_value).unwrap();

// JSON decode
let grid: Value = serde_json::from_str(&json_string).unwrap();
```

This enables Sandstar to serve both Zinc and JSON responses from the Haystack API, improving compatibility with modern clients.

---

## 7. Unit System

### C++ current

The C++ implementation stores units as bare strings (`const std::string unit` in `Num`). Unit validation is limited to character checking:

```cpp
bool Num::is_unit_name(const std::string& unit) {
    for (auto it = unit.begin(); it != unit.end(); ++it) {
        int c = *it;
        if (c > 31 && c < 128
            && !(c >= 'a' && c <= 'z')
            && !(c >= 'A' && c <= 'Z')
            && c != '_' && c != '$' && c != '%' && c != '/')
            return false;
    }
    return true;
}
```

### Rust replacement (`libhaystack::units`)

```rust
use libhaystack::units::*;

// Lookup a unit from the Haystack unit database
let fahrenheit: Unit = get_unit("°F").unwrap();
let celsius: Unit = get_unit("°C").unwrap();

// Create Number with validated unit
let temp = Value::make_number_unit(72.5, fahrenheit);

// Unit conversion (if supported by the unit database)
// Units carry dimension information for validation
```

**Rust improvements:**
- Full Project Haystack unit ontology (hundreds of units with dimensions)
- Units are typed structs, not bare strings
- Invalid unit names are caught at creation time
- Potential for unit conversion support

---

## 8. Complete Type Mapping Summary

| C++ Type | Lines (cpp+hpp) | Rust libhaystack Type | Module Path |
|----------|-----------------|----------------------|-------------|
| `Val` | 169 | `Value` (enum) | `haystack::val::Value` |
| `EmptyVal` | (in val) | `Value::Null` | `haystack::val::Value` |
| `Bool` | 125 | `Value::Bool(Bool)` | `haystack::val::Bool` |
| `Num` | 231 | `Value::Number(Number)` | `haystack::val::Number` |
| `Str` | 190 | `Value::Str(Str)` | `haystack::val::Str` |
| `Ref` | 188 | `Value::Ref(Ref)` | `haystack::val::Ref` |
| `Uri` | 148 | `Value::Uri(Uri)` | `haystack::val::Uri` |
| `Marker` | 120 | `Value::Marker` | `haystack::val::Value` |
| `Na` | 109 | `Value::Na` | `haystack::val::Value` |
| `Bin` | 142 | `Value::XStr(XStr)` | `haystack::val::XStr` |
| `Coord` | 221 | `Value::Coord(Coord)` | `haystack::val::Coord` |
| `Date` | 278 | `Value::Date(Date)` | `haystack::val::Date` |
| `Time` | 234 | `Value::Time(Time)` | `haystack::val::Time` |
| `DateTime` | 305 | `Value::DateTime(DateTime)` | `haystack::val::DateTime` |
| `XStr` | 209 | `Value::XStr(XStr)` | `haystack::val::XStr` |
| `List` | 165 | `Value::List(List)` | `haystack::val::List` |
| `Dict` | 560 | `Value::Dict(Dict)` / `Dict` | `haystack::val::Dict` |
| `Grid` | 626 | `Value::Grid(Grid)` / `Grid` | `haystack::val::Grid` |
| `Col` | 106 | (internal to Grid) | -- |
| `Row` | 294 | `Dict` (rows are Dicts) | `haystack::val::Dict` |
| `GridVal` | 104 | `Value::Grid(Grid)` | `haystack::val::Grid` |
| `GridView` | (in grid.hpp) | `&Grid` (Rust borrow) | -- |
| `HisItem` | 105 | `Dict` with ts/val keys | -- |
| `TimeZone` | 155 | `chrono_tz` (internal) | -- |
| `DateTimeRange` | 241 | Custom (~30 lines) | Sandstar-specific |
| `DictType` | 193 | `Dict` (proper nested) | `haystack::val::Dict` |
| `Filter` (13 classes) | 700 | `Filter` | `haystack::filter::Filter` |
| `ZincReader` | 997 | `zinc::decode` | `haystack::encoding::zinc::decode` |
| `ZincWriter` | 186 | `zinc::encode` | `haystack::encoding::zinc::encode` |
| `Tokenizer` | 497 | (internal to decoder) | -- |
| `Token` | 136 | (internal to decoder) | -- |
| `Proj` | 165 | Rust trait (~20 lines) | Sandstar-specific |
| **(none)** | -- | `Value::Remove` | Haystack 4 new |
| **(none)** | -- | `Value::Symbol(Symbol)` | Haystack 4 new |
| **(none)** | -- | JSON encoding | `haystack::encoding::json` |

---

## 9. Cargo.toml Configuration

```toml
[dependencies]
# Complete Haystack 4 type system, filter, zinc, json, units
libhaystack = "2.0"

# For selective feature compilation (smaller binary):
# libhaystack = { version = "2.0", default-features = false, features = ["encoders", "zinc"] }
```

Feature flags for optimized builds:
- `encoders` -- Include encoding/decoding support
- `zinc` -- Include Zinc format support
- `json` -- Include JSON format support (via Serde)
- `units` -- Include Project Haystack unit database
- `filter` -- Include filter parser and evaluator

On a constrained target like BeagleBone, selective features can produce binaries as small as ~12KB for just core types + zinc encoding.

---

## 10. Migration Checklist

### Phase 1: Type replacement (immediate)

- [ ] Add `libhaystack = "2.0"` to Cargo.toml
- [ ] Replace all `haystack::Val` usage with `libhaystack::val::Value`
- [ ] Replace all `haystack::Dict` usage with `libhaystack::val::Dict`
- [ ] Replace all `haystack::Grid` usage with `libhaystack::val::Grid`
- [ ] Replace all `haystack::Ref` usage with `libhaystack::val::Ref`
- [ ] Replace all `haystack::Num` usage with `libhaystack::val::Number`
- [ ] Replace all `haystack::Filter` usage with `libhaystack::filter::Filter`
- [ ] Replace `ZincReader::read_grid()` with `zinc::decode::from_str()`
- [ ] Replace `ZincWriter::grid_to_string()` with `zinc::encode::to_string()`
- [ ] Replace bare string units with `libhaystack::units::get_unit()`

### Phase 2: API adaptation

- [ ] Convert `Proj` abstract class to Rust trait
- [ ] Implement `DateTimeRange` as small Sandstar utility struct
- [ ] Convert `HisItem` usage to Dict-based Grid rows
- [ ] Add JSON encoding support to REST API responses
- [ ] Implement filter-based Grid querying for `read` ops

### Phase 3: Verification

- [ ] Port Zinc encoding test vectors from C++ to Rust
- [ ] Verify all channel tag names parse correctly
- [ ] Verify all sensor unit strings are in libhaystack unit database
- [ ] Test filter expressions used in Sandstar ops
- [ ] Benchmark Dict/Grid operations on BeagleBone

---

## 11. Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| libhaystack API breaking change | Cargo.toml pins `"2.0"`, semver protects | Pin exact version in production |
| Missing unit in unit database | Sensor reading has no unit | Audit all Sandstar units against libhaystack DB |
| Performance regression on ARM | Slower Dict access | BTreeMap vs ptr_map benchmarking on target |
| Haystack 3 vs 4 encoding differences | Client compatibility | Test with existing SkySpark clients |
| DateTimeRange not in libhaystack | Need custom code | ~30 lines Rust, well-defined scope |
| Filter path resolution (`->`) | Different Pather interface | Implement as closure in Sandstar |

---

## 12. References

- **libhaystack crate:** https://crates.io/crates/libhaystack
- **libhaystack GitHub:** https://github.com/j2inn/libhaystack
- **libhaystack docs.rs:** https://docs.rs/libhaystack/latest/libhaystack/
- **Value enum docs:** https://docs.rs/libhaystack/latest/libhaystack/haystack/val/value/enum.Value.html
- **Dict struct docs:** https://docs.rs/libhaystack/latest/libhaystack/haystack/val/dict/struct.Dict.html
- **Filter module docs:** https://docs.rs/libhaystack/latest/libhaystack/haystack/filter/index.html
- **Project Haystack spec:** https://project-haystack.org/doc/docHaystack/Kinds
- **J2 Innovations blog post:** https://www.j2inn.com/blog/a-new-open-source-library-for-project-haystack-rust
- **C++ source directory:** `shaystack/sandstar/sandstar/EacIo/src/EacIo/native/haystack/`
