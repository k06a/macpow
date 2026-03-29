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

/// Convert a `CFStringRef` to a Rust `String`. Returns `None` if null.
pub unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let cf: CFString = TCFType::wrap_under_get_rule(s);
    Some(cf.to_string())
}

/// Look up a key in a `CFDictionaryRef`. Returns null `CFTypeRef` if not found.
pub unsafe fn cfdict_get(dict: CFDictionaryRef, key: &str) -> CFTypeRef {
    let cf_key = CFString::new(key);
    CFDictionaryGetValue(dict, cf_key.as_concrete_TypeRef() as CFTypeRef)
}

/// Get an `i64` from a dictionary by key.
pub unsafe fn cfdict_get_i64(dict: CFDictionaryRef, key: &str) -> Option<i64> {
    let val = cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    let n: CFNumber = TCFType::wrap_under_get_rule(val as CFNumberRef);
    n.to_i64()
}

/// Get an `f64` from a dictionary by key.
pub unsafe fn cfdict_get_f64(dict: CFDictionaryRef, key: &str) -> Option<f64> {
    let val = cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    let n: CFNumber = TCFType::wrap_under_get_rule(val as CFNumberRef);
    n.to_f64()
}

/// Get a `bool` from a dictionary by key.
pub unsafe fn cfdict_get_bool(dict: CFDictionaryRef, key: &str) -> Option<bool> {
    let val = cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    let b: CFBoolean = TCFType::wrap_under_get_rule(val as _);
    Some(b == CFBoolean::true_value())
}

/// Get a `String` from a dictionary by key.
pub unsafe fn cfdict_get_string(dict: CFDictionaryRef, key: &str) -> Option<String> {
    let val = cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    cfstring_to_string(val as CFStringRef)
}

/// Get the length of a `CFArrayRef`.
pub unsafe fn cfarray_len(arr: CFArrayRef) -> isize {
    if arr.is_null() {
        return 0;
    }
    CFArrayGetCount(arr)
}

/// Get an element from a `CFArrayRef` by index.
pub unsafe fn cfarray_get(arr: CFArrayRef, idx: isize) -> CFTypeRef {
    CFArrayGetValueAtIndex(arr, idx)
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
