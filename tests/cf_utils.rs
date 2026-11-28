//! Hermetic tests for the type checks in `macpow::cf_utils`.
//!
//! Every test builds real CoreFoundation objects in-process, so none of them
//! needs hardware and all of them pass in a virtual machine.
//!
//! IORegistry and IOReport property types change between macOS versions and
//! Mac models. A getter that casts a value without checking its type rarely
//! fails an assertion: CoreFoundation raises "unrecognized selector" and aborts
//! the whole test binary. A run that dies with SIGABRT is therefore the
//! regression these tests guard against.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::{CFDictionary, CFMutableDictionary};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::string::CFStringRef;
use macpow::cf_utils::*;

/// Too long to be stored inside a tagged pointer.
const LONG_STRING: &str = "a string long enough that CoreFoundation allocates it on the heap";

/// A key that `fixture()` does not contain.
const ABSENT: &str = "absent";

/// The dictionary stored under `dict` and inside `array`.
fn nested() -> CFDictionary<CFString, CFType> {
    CFDictionary::from_CFType_pairs(&[(CFString::new("inner"), CFNumber::from(7i64).as_CFType())])
}

/// One value of every type macpow reads, keyed by what it is. Strings and
/// integers appear twice because CoreFoundation keeps small ones inside the
/// pointer itself (tagged pointers), and the checks must hold for both forms.
fn values() -> Vec<(&'static str, CFType)> {
    vec![
        ("short_string", CFString::new("en0").as_CFType()),
        ("long_string", CFString::new(LONG_STRING).as_CFType()),
        ("small_int", CFNumber::from(42i64).as_CFType()),
        ("big_int", CFNumber::from(i64::MAX).as_CFType()),
        ("float", CFNumber::from(1.5f64).as_CFType()),
        ("true", CFBoolean::true_value().as_CFType()),
        ("false", CFBoolean::false_value().as_CFType()),
        (
            "data",
            CFData::from_buffer(&[1, 2, 3, 4, 5, 6, 7, 8]).as_CFType(),
        ),
        ("dict", nested().as_CFType()),
        (
            "array",
            CFArray::from_CFTypes(&[nested(), nested()]).as_CFType(),
        ),
    ]
}

/// `values()` as a mutable dictionary, which is what
/// `IORegistryEntryCreateCFProperties` hands to macpow.
fn fixture() -> CFMutableDictionary<CFString, CFType> {
    let pairs: Vec<(CFString, CFType)> = values()
        .into_iter()
        .map(|(key, value)| (CFString::new(key), value))
        .collect();
    CFMutableDictionary::from_CFType_pairs(&pairs)
}

fn as_dict(dict: &CFMutableDictionary<CFString, CFType>) -> CFDictionaryRef {
    dict.as_concrete_TypeRef() as CFDictionaryRef
}

/// Every key of `fixture()` except `right`, plus one that is missing.
fn keys_except(right: &[&str]) -> Vec<&'static str> {
    values()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| !right.contains(key))
        .chain([ABSENT])
        .collect()
}

#[test]
fn i64_getter_reads_numbers_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block.
    unsafe {
        assert_eq!(cfdict_get_i64(d, "small_int"), Some(42));
        assert_eq!(cfdict_get_i64(d, "big_int"), Some(i64::MAX));
        // `float` is a number too; whether 1.5 converts is CFNumber's call.
        for key in keys_except(&["small_int", "big_int", "float"]) {
            assert_eq!(cfdict_get_i64(d, key), None, "{key}");
        }
    }
}

#[test]
fn f64_getter_reads_numbers_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block.
    unsafe {
        // Integer values count: battery.rs reads integer properties as f64.
        assert_eq!(cfdict_get_f64(d, "small_int"), Some(42.0));
        assert_eq!(cfdict_get_f64(d, "float"), Some(1.5));
        for key in keys_except(&["small_int", "big_int", "float"]) {
            assert_eq!(cfdict_get_f64(d, key), None, "{key}");
        }
    }
}

#[test]
fn bool_getter_reads_booleans_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block.
    unsafe {
        assert_eq!(cfdict_get_bool(d, "true"), Some(true));
        assert_eq!(cfdict_get_bool(d, "false"), Some(false));
        for key in keys_except(&["true", "false"]) {
            assert_eq!(cfdict_get_bool(d, key), None, "{key}");
        }
    }
}

#[test]
fn string_getter_reads_strings_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block.
    unsafe {
        assert_eq!(cfdict_get_string(d, "short_string").as_deref(), Some("en0"));
        assert_eq!(
            cfdict_get_string(d, "long_string").as_deref(),
            Some(LONG_STRING)
        );
        for key in keys_except(&["short_string", "long_string"]) {
            assert_eq!(cfdict_get_string(d, key), None, "{key}");
        }
    }
}

#[test]
fn dict_getter_returns_dictionaries_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block,
    // and so is the nested dictionary it holds.
    unsafe {
        let inner = cfdict_get_dict(d, "dict").expect("a dictionary value");
        assert_eq!(cfdict_get_i64(inner, "inner"), Some(7));
        for key in keys_except(&["dict"]) {
            assert_eq!(cfdict_get_dict(d, key), None, "{key}");
        }
    }
}

/// metrics.rs reaches nested dictionaries as `cfdict_get(value as _, key)`.
#[test]
fn lookup_in_a_value_that_is_not_a_dictionary_finds_nothing() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block,
    // and every other pointer below is null or one of its values.
    unsafe {
        let inner = cfdict_get(d, "dict") as CFDictionaryRef;
        assert_eq!(cfdict_get_i64(inner, "inner"), Some(7));
        for key in keys_except(&["dict"]) {
            let not_a_dict = cfdict_get(d, key) as CFDictionaryRef;
            assert!(cfdict_get(not_a_dict, "inner").is_null(), "{key}");
            assert_eq!(cfdict_get_i64(not_a_dict, "inner"), None, "{key}");
        }
    }
}

/// battery.rs and ioreport.rs iterate `cfdict_get(dict, key) as CFArrayRef`.
#[test]
fn array_len_is_zero_for_everything_but_an_array() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block,
    // every other pointer below is null or one of its values, and
    // `cfarray_get` is only called below the length just read.
    unsafe {
        let arr = cfdict_get(d, "array") as CFArrayRef;
        assert_eq!(cfarray_len(arr), 2);
        for i in 0..cfarray_len(arr) {
            let entry = cfarray_get(arr, i) as CFDictionaryRef;
            assert_eq!(cfdict_get_i64(entry, "inner"), Some(7));
        }
        for key in keys_except(&["array"]) {
            let not_an_array = cfdict_get(d, key) as CFArrayRef;
            assert_eq!(cfarray_len(not_an_array), 0, "{key}");
        }
    }
}

/// ioreport.rs and peripherals.rs convert values cast to `CFStringRef`.
#[test]
fn cfstring_to_string_converts_strings_and_nothing_else() {
    let dict = fixture();
    let d = as_dict(&dict);
    // SAFETY: `d` is a live dictionary owned by `dict` for the whole block,
    // and every other pointer below is null or one of its values.
    unsafe {
        let short = cfdict_get(d, "short_string") as CFStringRef;
        assert_eq!(cfstring_to_string(short).as_deref(), Some("en0"));
        let long = cfdict_get(d, "long_string") as CFStringRef;
        assert_eq!(cfstring_to_string(long).as_deref(), Some(LONG_STRING));
        for key in keys_except(&["short_string", "long_string"]) {
            let not_a_string = cfdict_get(d, key) as CFStringRef;
            assert_eq!(cfstring_to_string(not_a_string), None, "{key}");
        }
    }
}
