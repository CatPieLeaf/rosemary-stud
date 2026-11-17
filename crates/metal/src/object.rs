use std::ffi::CStr;

use objc::class::Class;
use objc::object::Id;

#[repr(C)]
pub(crate) struct Object<T> {
    isa: *const Class,
    body: T,
}

/// Declare a Metal class whose instances carry a `T`.
///
/// Each instance also keeps its reference count in a word after the body: a
/// frame retains and releases Metal objects by the thousand, and the runtime's
/// shared table took a lock for every one. Only the runtime allocates a Metal
/// object, which is what [`Class::count_inline`] requires.
pub(crate) fn register<T>(name: &CStr) -> &'static Class {
    let fresh = objc::class::lookup(name).is_none();
    let word = size_of::<usize>();
    let size = size_of::<Object<T>>().next_multiple_of(word) + word;
    let class = corefoundation::base::register(name, None, size, false);
    if fresh && class.instance_size() == size {
        class.count_inline();
    }
    class
}

/// Allocate one instance of a declared class and install its body.
pub(crate) fn make<T>(name: &CStr, body: T) -> Id {
    make_with(name, move || body)
}

/// [`make`] with a body built only once its memory exists, so that a large one
/// - an encoder's binding tables are kilobytes - is written straight into the
/// instance rather than built elsewhere and copied in.
pub(crate) fn make_with<T>(name: &CStr, body: impl FnOnce() -> T) -> Id {
    let class = objc::class::lookup(name).expect("Metal is bootstrapped");
    assert!(
        class.instance_size() >= size_of::<Object<T>>(),
        "{} was registered smaller than its body",
        class.name_str()
    );
    // The body is written whole below, so the memory is not zeroed first.
    let object = objc::object::create_instance_unzeroed(class) as *mut Object<T>;
    if object.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a fresh instance of the layout `register::<T>` declared, whose
    // body is uninitialized memory this call establishes without dropping.
    unsafe { std::ptr::addr_of_mut!((*object).body).write(body()) };
    object as Id
}

/// Install the body of an instance the Objective-C runtime allocated, which
/// is how a class a guest makes with `+alloc` and `-init` gets one.
///
/// # Safety
///
/// `receiver` must be an instance of a class declared for this `T` whose body
/// has not been established, or has been dropped since.
pub(crate) unsafe fn init<T>(receiver: Id, body: T) {
    // SAFETY: the caller guarantees the layout; the old body is not dropped,
    // because there is none.
    unsafe { std::ptr::addr_of_mut!((*(receiver as *mut Object<T>)).body).write(body) };
}

/// The body of an object of a class declared with `register::<T>`.
///
/// # Safety
///
/// `receiver` must be an instance of a class declared for this `T`. Every
/// caller is a method implementation registered on exactly that class.
pub(crate) unsafe fn body<'a, T>(receiver: Id) -> &'a mut T {
    // SAFETY: the caller guarantees the receiver's layout.
    unsafe { &mut (*(receiver as *mut Object<T>)).body }
}

/// Read a live body's fields without claiming exclusive access.
///
/// # Safety
/// `receiver` has this layout; concurrent mutation must use interior mutability.
pub(crate) unsafe fn body_ref<'a, T>(receiver: Id) -> &'a T {
    unsafe { &(*(receiver as *const Object<T>)).body }
}

/// Whether `object` is an instance of `name` or one of its subclasses.
pub(crate) fn is_kind(object: Id, name: &CStr) -> bool {
    let Some(class) = objc::class::lookup(name) else {
        return false;
    };
    // SAFETY: null, or an object this runtime allocated.
    unsafe { objc::object::class_of(object) }.is_some_and(|actual| {
        actual
            .chain()
            .any(|candidate| std::ptr::eq(candidate, class))
    })
}

macro_rules! methods {
    ($class:expr, { $( $sel:literal => $imp:expr, $types:literal );* $(;)? }) => {{
        let class: &'static objc::class::Class = $class;
        $(
            class.replace_method(
                objc::sel::get($sel),
                $imp as *const std::ffi::c_void,
                $types,
            );
        )*
    }};
}

pub(crate) use methods;
