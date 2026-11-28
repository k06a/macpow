use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::{
    CFDictionaryCreateMutableCopy, CFDictionaryGetValue, CFDictionaryRef, CFMutableDictionaryRef,
};
use core_foundation_sys::number::CFNumberRef;
use core_foundation_sys::string::CFStringRef;

use core_foundation_sys::array::CFArrayGetTypeID;
use core_foundation_sys::base::{CFGetTypeID, CFTypeID};
use core_foundation_sys::dictionary::CFDictionaryGetTypeID;
use core_foundation_sys::number::{CFBooleanGetTypeID, CFNumberGetTypeID};
use core_foundation_sys::string::CFStringGetTypeID;

/// Whether `obj` is a non-null CoreFoundation object of the type `type_id`.
///
/// The values read here come from IORegistry and IOReport properties, whose
/// types differ between macOS versions and Mac models. Handing a value of the
/// wrong type to a typed CoreFoundation call raises an Objective-C exception
/// ("unrecognized selector") that aborts the whole process, so every function
/// below asks first and reports a mismatch the same way as a missing key.
unsafe fn is_type(obj: CFTypeRef, type_id: CFTypeID) -> bool {
    // SAFETY: the caller passes null or a live CoreFoundation object, and
    // `CFGetTypeID` accepts any non-null object, tagged pointers included.
    !obj.is_null() && CFGetTypeID(obj) == type_id
}

/// Convert a `CFStringRef` to a Rust `String`. Returns `None` if null or not
/// a `CFString`: callers pass dictionary keys and values cast to `CFStringRef`.
pub unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if !is_type(s as CFTypeRef, CFStringGetTypeID()) {
        return None;
    }
    // SAFETY: `s` was just checked to be a non-null `CFString`.
    let cf: CFString = TCFType::wrap_under_get_rule(s);
    Some(cf.to_string())
}

/// Look up a key in a `CFDictionaryRef`. Returns null `CFTypeRef` if not found,
/// or if `dict` is null or not a dictionary: callers reach nested dictionaries
/// by casting the value of an earlier lookup.
pub unsafe fn cfdict_get(dict: CFDictionaryRef, key: &str) -> CFTypeRef {
    if !is_type(dict as CFTypeRef, CFDictionaryGetTypeID()) {
        return std::ptr::null();
    }
    let cf_key = CFString::new(key);
    // SAFETY: `dict` was just checked to be a non-null `CFDictionary`, and
    // `cf_key` outlives the call.
    CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as CFTypeRef)
}

/// Get an `i64` from a dictionary by key. Returns `None` if missing or not a
/// `CFNumber`.
pub unsafe fn cfdict_get_i64(dict: CFDictionaryRef, key: &str) -> Option<i64> {
    let val = cfdict_get(dict, key);
    if !is_type(val, CFNumberGetTypeID()) {
        return None;
    }
    // SAFETY: `val` was just checked to be a non-null `CFNumber`.
    let n: CFNumber = TCFType::wrap_under_get_rule(val as CFNumberRef);
    n.to_i64()
}

/// Get an `f64` from a dictionary by key. Returns `None` if missing or not a
/// `CFNumber`.
pub unsafe fn cfdict_get_f64(dict: CFDictionaryRef, key: &str) -> Option<f64> {
    let val = cfdict_get(dict, key);
    if !is_type(val, CFNumberGetTypeID()) {
        return None;
    }
    // SAFETY: `val` was just checked to be a non-null `CFNumber`.
    let n: CFNumber = TCFType::wrap_under_get_rule(val as CFNumberRef);
    n.to_f64()
}

/// Get a `bool` from a dictionary by key. Returns `None` if missing or not a
/// `CFBoolean`.
pub unsafe fn cfdict_get_bool(dict: CFDictionaryRef, key: &str) -> Option<bool> {
    let val = cfdict_get(dict, key);
    if !is_type(val, CFBooleanGetTypeID()) {
        return None;
    }
    // SAFETY: `val` was just checked to be a non-null `CFBoolean`.
    let b: CFBoolean = TCFType::wrap_under_get_rule(val as _);
    Some(b == CFBoolean::true_value())
}

/// Get a `String` from a dictionary by key. Returns `None` if missing or not a
/// `CFString`.
pub unsafe fn cfdict_get_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
    // `cfstring_to_string` rejects null and anything that is not a string.
    cfstring_to_string(cfdict_get(dict, key) as CFStringRef)
}

/// Get the length of a `CFArrayRef`. Returns 0 if null or not a `CFArray`:
/// callers pass a dictionary value cast to `CFArrayRef`, and a length of 0
/// keeps their loops from ever indexing it.
pub unsafe fn cfarray_len(arr: CFArrayRef) -> isize {
    if !is_type(arr as CFTypeRef, CFArrayGetTypeID()) {
        return 0;
    }
    // SAFETY: `arr` was just checked to be a non-null `CFArray`.
    CFArrayGetCount(arr)
}

/// Get an element from a `CFArrayRef` by index. Only call it with
/// `idx < cfarray_len(arr)`: that bound is also what proves `arr` is an array.
pub unsafe fn cfarray_get(arr: CFArrayRef, idx: isize) -> CFTypeRef {
    CFArrayGetValueAtIndex(arr, idx)
}

/// Get a nested `CFDictionaryRef` from a dictionary by key. Returns `None` if
/// missing or not a `CFDictionary`.
pub unsafe fn cfdict_get_dict(dict: CFDictionaryRef, key: &str) -> Option<CFDictionaryRef> {
    let val = cfdict_get(dict, key);
    if !is_type(val, CFDictionaryGetTypeID()) {
        return None;
    }
    Some(val as CFDictionaryRef)
}

/// Create a mutable copy of a `CFDictionaryRef`.
pub unsafe fn cfdict_mutable_copy(dict: CFDictionaryRef) -> CFMutableDictionaryRef {
    CFDictionaryCreateMutableCopy(std::ptr::null(), 0, dict)
}

/// Release a CoreFoundation object. No-op if null.
pub unsafe fn cf_release(obj: CFTypeRef) {
    if !obj.is_null() {
        CFRelease(obj);
    }
}
