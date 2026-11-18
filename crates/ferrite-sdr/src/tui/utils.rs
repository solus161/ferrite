/// A getter handing out a clone of an `Rc<Cell<_>>` field, for a widget that
/// has to *observe* the value rather than copy it once at construction.
///
/// Defined here at the top rather than beside the `impl` that uses it:
/// `macro_rules!` is textually scoped, so unlike every other item it is only in
/// scope *below* its definition.
///
/// The type has to be passed in — the expansion names it in the return type,
/// and a macro cannot read the field's declared type. Sharing a name with the
/// field is fine: methods and fields live in separate namespaces, so
/// `self.center_freq` and `self.center_freq()` coexist.
macro_rules! get_attr_clone {
    ($attr:ident, $ty:ty) => {
        pub fn $attr(&self) -> Rc<Cell<$ty>> {
            self.$attr.clone()
        }
    };
}

pub(crate) use get_attr_clone;
