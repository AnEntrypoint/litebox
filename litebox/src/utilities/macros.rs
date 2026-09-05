// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

/// Define a `#[repr($int_ty)]` enum and auto-generate a typed conversion method.
///
/// Not replaced with `num_enum::{TryFromPrimitive, IntoPrimitive}`: of this macro's two call sites,
/// `litebox/src/fs/nine_p/fcall.rs` is owned by other concurrent work at the time of this audit
/// pass and cannot be touched here, so the macro itself cannot be removed regardless of what
/// happens to the other call site (`net/errors.rs`). Converting only `net/errors.rs` would add
/// `num_enum` as a dependency while leaving `fcall.rs` on this macro, i.e. two competing
/// enum-from-int mechanisms for no net reduction in code -- not a real improvement over the
/// current single (if reinvented) macro.
///
/// # Example
/// ```ignore
/// repr_enum! {
///     #[derive(Copy, Clone, Debug)]
///     enum Color: u8, from_u8 {
///         Red   = 1,
///         Green = 2,
///         Blue  = 3,
///     }
/// }
/// assert_eq!(Color::from_u8(2), Some(Color::Green));
/// ```
macro_rules! repr_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident : $int_ty:ty, $from_fn:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident = $value:expr
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr($int_ty)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant = $value,
            )*
        }

        impl $name {
            /// Convert a raw integer to the enum, returning `None` for unknown values.
            $vis fn $from_fn(v: $int_ty) -> Option<Self> {
                match v {
                    $( $value => Some(Self::$variant), )*
                    _ => None,
                }
            }
        }
    };
}
pub(crate) use repr_enum;
