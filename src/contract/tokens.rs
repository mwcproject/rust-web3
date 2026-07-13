//! Contract Functions Output types.

use crate::{
    contract::error::Error,
    types::{Address, Bytes, BytesArray, H256, U128, U256},
};
use ethabi::{Token, Uint};

/// Output type possible to deserialize from Contract ABI
pub trait Detokenize {
    /// Creates a new instance from parsed ABI tokens.
    fn from_tokens(tokens: Vec<Token>) -> Result<Self, Error>
    where
        Self: Sized;
}

impl<T: Tokenizable> Detokenize for T {
    fn from_tokens(tokens: Vec<Token>) -> Result<Self, Error> {
        let [token] = tokens.try_into().map_err(|tokens: Vec<Token>| {
            Error::InvalidOutputType(format!(
                "Expected single element, got a list: {:?}",
                tokens
            ))
        })?;
        Self::from_token(token)
    }
}

macro_rules! impl_output {
  ($num: expr, $( $ty: ident , )+) => {
    impl<$($ty, )+> Detokenize for ($($ty,)+) where
      $(
        $ty: Tokenizable,
      )+
    {
      fn from_tokens(mut tokens: Vec<Token>) -> Result<Self, Error> {
        if tokens.len() != $num {
          return Err(Error::InvalidOutputType(format!(
            "Expected {} elements, got a list of {}: {:?}",
            $num,
            tokens.len(),
            tokens
          )));
        }
        let mut it = tokens.drain(..);
        // The exact-length guard proves every extraction is present; keep extraction
        // fallible so future macro changes cannot turn malformed ABI output into a panic.
        Ok(($(
          $ty::from_token(it.next().ok_or_else(|| Error::InvalidOutputType(
            "Token list ended before all tuple elements were decoded".into()
          ))?)?,
        )+))
      }
    }
  }
}

impl_output!(1, A,);
impl_output!(2, A, B,);
impl_output!(3, A, B, C,);
impl_output!(4, A, B, C, D,);
impl_output!(5, A, B, C, D, E,);
impl_output!(6, A, B, C, D, E, F,);
impl_output!(7, A, B, C, D, E, F, G,);
impl_output!(8, A, B, C, D, E, F, G, H,);
impl_output!(9, A, B, C, D, E, F, G, H, I,);
impl_output!(10, A, B, C, D, E, F, G, H, I, J,);
impl_output!(11, A, B, C, D, E, F, G, H, I, J, K,);
impl_output!(12, A, B, C, D, E, F, G, H, I, J, K, L,);
impl_output!(13, A, B, C, D, E, F, G, H, I, J, K, L, M,);
impl_output!(14, A, B, C, D, E, F, G, H, I, J, K, L, M, N,);
impl_output!(15, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O,);
impl_output!(16, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P,);
/// Tokens conversion trait
pub trait Tokenize {
    /// Convert to list of tokens
    fn into_tokens(self) -> Vec<Token>;
}

impl Tokenize for &[Token] {
    fn into_tokens(self) -> Vec<Token> {
        self.to_vec()
    }
}

impl<T: Tokenizable> Tokenize for T {
    fn into_tokens(self) -> Vec<Token> {
        vec![self.into_token()]
    }
}

impl Tokenize for () {
    fn into_tokens(self) -> Vec<Token> {
        vec![]
    }
}

macro_rules! impl_tokens {
  ($( $ty: ident : $no: tt, )+) => {
    impl<$($ty, )+> Tokenize for ($($ty,)+) where
      $(
        $ty: Tokenizable,
      )+
    {
      fn into_tokens(self) -> Vec<Token> {
        vec![
          $( self.$no.into_token(), )+
        ]
      }
    }
  }
}

impl_tokens!(A:0, );
impl_tokens!(A:0, B:1, );
impl_tokens!(A:0, B:1, C:2, );
impl_tokens!(A:0, B:1, C:2, D:3, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, L:11, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, L:11, M:12, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, L:11, M:12, N:13, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, L:11, M:12, N:13, O:14, );
impl_tokens!(A:0, B:1, C:2, D:3, E:4, F:5, G:6, H:7, I:8, J:9, K:10, L:11, M:12, N:13, O:14, P:15, );

/// Simplified output type for single value.
pub trait Tokenizable {
    /// Converts a `Token` into expected type.
    fn from_token(token: Token) -> Result<Self, Error>
    where
        Self: Sized;
    /// Converts a specified type back into token.
    fn into_token(self) -> Token;
}

impl Tokenizable for Token {
    fn from_token(token: Token) -> Result<Self, Error> {
        Ok(token)
    }
    fn into_token(self) -> Token {
        self
    }
}

impl Tokenizable for String {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::String(s) => Ok(s),
            other => Err(Error::InvalidOutputType(format!("Expected `String`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::String(self)
    }
}

impl Tokenizable for Bytes {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::Bytes(s) => Ok(s.into()),
            other => Err(Error::InvalidOutputType(format!("Expected `Bytes`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::Bytes(self.0)
    }
}

impl Tokenizable for H256 {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::FixedBytes(s) => {
                if s.len() != 32 {
                    return Err(Error::InvalidOutputType(format!("Expected `H256`, got {:?}", s)));
                }
                let mut data = [0; 32];
                data.copy_from_slice(&s);
                Ok(data.into())
            }
            other => Err(Error::InvalidOutputType(format!("Expected `H256`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::FixedBytes(self.as_ref().to_vec())
    }
}

impl Tokenizable for Address {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::Address(data) => Ok(Address::from_slice(data.as_bytes())),
            other => Err(Error::InvalidOutputType(format!("Expected `Address`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::Address(ethabi::Address::from_slice(self.as_bytes()))
    }
}

macro_rules! eth_uint_tokenizable {
    ($uint: ident, $name: expr) => {
        impl Tokenizable for $uint {
            fn from_token(token: Token) -> Result<Self, Error> {
                match token {
                    Token::Int(data) | Token::Uint(data) => {
                        let mut bytes = [0; 32];
                        data.to_big_endian(&mut bytes);
                        let data = U256::from_big_endian(&bytes);
                        let converted = ::std::convert::TryInto::try_into(data).map_err(|_| {
                            Error::InvalidOutputType(format!("ABI integer does not fit `{}`", $name))
                        })?;
                        Ok(converted)
                    }
                    other => Err(Error::InvalidOutputType(format!("Expected `{}`, got {:?}", $name, other)).into()),
                }
            }

            fn into_token(self) -> Token {
                let data: U256 = self.into();
                Token::Uint(Uint::from_big_endian(&data.to_big_endian()))
            }
        }
    };
}

eth_uint_tokenizable!(U256, "U256");
eth_uint_tokenizable!(U128, "U128");

macro_rules! signed_int_tokenizable {
    ($int: ident) => {
        impl Tokenizable for $int {
            fn from_token(token: Token) -> Result<Self, Error> {
                match token {
                    Token::Uint(data) => {
                        let max = Uint::from(u128::from($int::MAX.unsigned_abs()));
                        if data > max {
                            return Err(Error::InvalidOutputType(format!(
                                "ABI integer does not fit `{}`",
                                stringify!($int)
                            )));
                        }
                        $int::try_from(data.low_u128()).map_err(|_| {
                            Error::InvalidOutputType(format!(
                                "ABI integer does not fit `{}`",
                                stringify!($int)
                            ))
                        })
                    }
                    Token::Int(data) => {
                        let negative = data.bit(255);
                        let max = Uint::from(u128::from($int::MAX.unsigned_abs()));
                        let min_magnitude = Uint::from(u128::from($int::MIN.unsigned_abs()));
                        if negative {
                            // With bit 255 set, `!data` is at most 2^255 - 1, so adding one
                            // cannot overflow this 256-bit value.
                            let (magnitude, _) = (!data).overflowing_add(Uint::one());
                            if magnitude > min_magnitude {
                                return Err(Error::InvalidOutputType(format!(
                                    "ABI integer does not fit `{}`",
                                    stringify!($int)
                                )));
                            }
                            if magnitude == min_magnitude {
                                Ok($int::MIN)
                            } else {
                                let magnitude = $int::try_from(magnitude.low_u128()).map_err(|_| {
                                    Error::InvalidOutputType(format!(
                                        "ABI integer does not fit `{}`",
                                        stringify!($int)
                                    ))
                                })?;
                                // Equality with `MIN.unsigned_abs()` was handled above, so this
                                // magnitude is at most `MAX` and negation cannot overflow.
                                #[allow(
                                    clippy::arithmetic_side_effects,
                                    reason = "the minimum magnitude was handled, so this value is at most MAX"
                                )]
                                Ok(-magnitude)
                            }
                        } else if data > max {
                            Err(Error::InvalidOutputType(format!(
                                "ABI integer does not fit `{}`",
                                stringify!($int)
                            )))
                        } else {
                            $int::try_from(data.low_u128()).map_err(|_| {
                                Error::InvalidOutputType(format!(
                                    "ABI integer does not fit `{}`",
                                    stringify!($int)
                                ))
                            })
                        }
                    }
                    other => Err(Error::InvalidOutputType(format!(
                        "Expected `{}`, got {:?}",
                        stringify!($int),
                        other
                    ))),
                }
            }

            fn into_token(self) -> Token {
                let data = if self < 0 {
                    // Modular subtraction from zero constructs the canonical 256-bit
                    // two's-complement representation; the underflow flag is intentional.
                    Uint::zero()
                        .overflowing_sub(Uint::from(u128::from(self.unsigned_abs())))
                        .0
                } else {
                    Uint::from(u128::from(self.unsigned_abs()))
                };
                Token::Int(data)
            }
        }
    };
}

macro_rules! unsigned_int_tokenizable {
    ($int: ident) => {
        impl Tokenizable for $int {
            fn from_token(token: Token) -> Result<Self, Error> {
                match token {
                    Token::Int(data) | Token::Uint(data) => {
                        let max = Uint::from(u128::from($int::MAX));
                        if data > max {
                            return Err(Error::InvalidOutputType(format!(
                                "ABI integer does not fit `{}`",
                                stringify!($int)
                            )));
                        }
                        $int::try_from(data.low_u128()).map_err(|_| {
                            Error::InvalidOutputType(format!("ABI integer does not fit `{}`", stringify!($int)))
                        })
                    }
                    other => Err(Error::InvalidOutputType(format!(
                        "Expected `{}`, got {:?}",
                        stringify!($int),
                        other
                    ))),
                }
            }

            fn into_token(self) -> Token {
                Token::Uint(Uint::from(u128::from(self)))
            }
        }
    };
}

signed_int_tokenizable!(i8);
signed_int_tokenizable!(i16);
signed_int_tokenizable!(i32);
signed_int_tokenizable!(i64);
signed_int_tokenizable!(i128);
unsigned_int_tokenizable!(u8);
unsigned_int_tokenizable!(u16);
unsigned_int_tokenizable!(u32);
unsigned_int_tokenizable!(u64);
unsigned_int_tokenizable!(u128);

impl Tokenizable for bool {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::Bool(data) => Ok(data),
            other => Err(Error::InvalidOutputType(format!("Expected `bool`, got {:?}", other))),
        }
    }
    fn into_token(self) -> Token {
        Token::Bool(self)
    }
}

/// Marker trait for `Tokenizable` types that are can tokenized to and from a
/// `Token::Array` and `Token:FixedArray`.
pub trait TokenizableItem: Tokenizable {}

macro_rules! tokenizable_item {
    ($($type: ty,)*) => {
        $(
            impl TokenizableItem for $type {}
        )*
    };
}

tokenizable_item! {
    Token, String, Address, H256, U256, U128, bool, BytesArray, Vec<u8>,
    i8, i16, i32, i64, i128, u16, u32, u64, u128,
}

impl Tokenizable for BytesArray {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::FixedArray(tokens) | Token::Array(tokens) => {
                let bytes = tokens
                    .into_iter()
                    .map(Tokenizable::from_token)
                    .collect::<Result<Vec<u8>, Error>>()?;
                Ok(Self(bytes))
            }
            other => Err(Error::InvalidOutputType(format!("Expected `Array`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::Array(self.0.into_iter().map(Tokenizable::into_token).collect())
    }
}

impl Tokenizable for Vec<u8> {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::Bytes(data) => Ok(data),
            Token::FixedBytes(data) => Ok(data),
            other => Err(Error::InvalidOutputType(format!("Expected `bytes`, got {:?}", other))),
        }
    }
    fn into_token(self) -> Token {
        Token::Bytes(self)
    }
}

impl<T: TokenizableItem> Tokenizable for Vec<T> {
    fn from_token(token: Token) -> Result<Self, Error> {
        match token {
            Token::FixedArray(tokens) | Token::Array(tokens) => {
                tokens.into_iter().map(Tokenizable::from_token).collect()
            }
            other => Err(Error::InvalidOutputType(format!("Expected `Array`, got {:?}", other))),
        }
    }

    fn into_token(self) -> Token {
        Token::Array(self.into_iter().map(Tokenizable::into_token).collect())
    }
}

impl<T: TokenizableItem> TokenizableItem for Vec<T> {}

macro_rules! impl_fixed_types {
    ($num: expr) => {
        impl Tokenizable for [u8; $num] {
            fn from_token(token: Token) -> Result<Self, Error> {
                match token {
                    Token::FixedBytes(bytes) => {
                        if bytes.len() != $num {
                            return Err(Error::InvalidOutputType(format!(
                                "Expected `FixedBytes({})`, got FixedBytes({})",
                                $num,
                                bytes.len()
                            )));
                        }

                        let mut arr = [0; $num];
                        arr.copy_from_slice(&bytes);
                        Ok(arr)
                    }
                    other => Err(
                        Error::InvalidOutputType(format!("Expected `FixedBytes({})`, got {:?}", $num, other)).into(),
                    ),
                }
            }

            fn into_token(self) -> Token {
                Token::FixedBytes(self.to_vec())
            }
        }

        impl TokenizableItem for [u8; $num] {}

        impl<T: TokenizableItem + Clone> Tokenizable for [T; $num] {
            fn from_token(token: Token) -> Result<Self, Error> {
                match token {
                    Token::FixedArray(tokens) => {
                        if tokens.len() != $num {
                            return Err(Error::InvalidOutputType(format!(
                                "Expected `FixedArray({})`, got FixedArray({})",
                                $num,
                                tokens.len()
                            )));
                        }

                        let values = tokens
                            .into_iter()
                            .map(T::from_token)
                            .collect::<Result<Vec<_>, _>>()?;
                        // One decoded value is produced per validated input token. Keep the
                        // conversion fallible instead of relying on that invariant with a panic.
                        values.try_into().map_err(|values: Vec<T>| {
                            Error::InvalidOutputType(format!(
                                "Expected `FixedArray({})`, got FixedArray({})",
                                $num,
                                values.len()
                            ))
                        })
                    }
                    other => Err(
                        Error::InvalidOutputType(format!("Expected `FixedArray({})`, got {:?}", $num, other)).into(),
                    ),
                }
            }

            fn into_token(self) -> Token {
                Token::FixedArray(self.into_iter().map(T::into_token).collect())
            }
        }

        impl<T: TokenizableItem + Clone> TokenizableItem for [T; $num] {}
    };
}

impl_fixed_types!(1);
impl_fixed_types!(2);
impl_fixed_types!(3);
impl_fixed_types!(4);
impl_fixed_types!(5);
impl_fixed_types!(6);
impl_fixed_types!(7);
impl_fixed_types!(8);
impl_fixed_types!(9);
impl_fixed_types!(10);
impl_fixed_types!(11);
impl_fixed_types!(12);
impl_fixed_types!(13);
impl_fixed_types!(14);
impl_fixed_types!(15);
impl_fixed_types!(16);
impl_fixed_types!(32);
impl_fixed_types!(64);
impl_fixed_types!(80);
impl_fixed_types!(128);
impl_fixed_types!(256);
impl_fixed_types!(512);
impl_fixed_types!(1024);

#[cfg(test)]
mod tests {
    use super::{Detokenize, Tokenizable};
    use crate::types::{Address, BytesArray, U128, U256};
    use ethabi::{Token, Uint};
    use hex_literal::hex;

    fn output<R: Detokenize>() -> R {
        unimplemented!()
    }

    #[test]
    #[ignore]
    fn should_be_able_to_compile() {
        let _tokens: Vec<Token> = output();
        let _uint: U256 = output();
        let _address: Address = output();
        let _string: String = output();
        let _bool: bool = output();
        let _bytes: Vec<u8> = output();
        let _bytes_array: BytesArray = output();

        let _pair: (U256, bool) = output();
        let _vec: Vec<U256> = output();
        let _array: [U256; 4] = output();
        let _bytes: Vec<[[u8; 1]; 64]> = output();

        let _mixed: (Vec<Vec<u8>>, [U256; 4], Vec<U256>, U256) = output();

        let _ints: (i8, i16, i32, i64, i128) = output();
        let _uints: (u16, u32, u64, u128) = output();
    }

    #[test]
    fn should_decode_array_of_fixed_bytes() {
        // byte[8][]
        let tokens = vec![Token::FixedArray(vec![
            Token::FixedBytes(hex!("01").into()),
            Token::FixedBytes(hex!("02").into()),
            Token::FixedBytes(hex!("03").into()),
            Token::FixedBytes(hex!("04").into()),
            Token::FixedBytes(hex!("05").into()),
            Token::FixedBytes(hex!("06").into()),
            Token::FixedBytes(hex!("07").into()),
            Token::FixedBytes(hex!("08").into()),
        ])];
        let data: [[u8; 1]; 8] = Detokenize::from_tokens(tokens).unwrap();
        assert_eq!(data[0][0], 1);
        assert_eq!(data[1][0], 2);
        assert_eq!(data[2][0], 3);
        assert_eq!(data[7][0], 8);
    }

    #[test]
    fn should_decode_array_of_bytes() {
        let token = Token::Array(vec![Token::Uint(Uint::from(0)), Token::Uint(Uint::from(1))]);
        let data: BytesArray = Tokenizable::from_token(token).unwrap();
        assert_eq!(data.0[0], 0);
        assert_eq!(data.0[1], 1);
    }

    #[test]
    fn should_roundtrip_upgraded_ethereum_types_through_ethabi_tokens() {
        let address = Address::from_low_u64_be(0x1234);
        assert_eq!(Address::from_token(address.into_token()).unwrap(), address);

        let uint256 = U256::MAX - 42;
        assert_eq!(U256::from_token(uint256.into_token()).unwrap(), uint256);

        let uint128 = U128::MAX - 42;
        assert_eq!(U128::from_token(uint128.into_token()).unwrap(), uint128);
    }

    #[test]
    fn should_reject_abi_integers_outside_the_requested_type() {
        let above_u128 = Uint::from(1_u8) << 128;
        assert!(U128::from_token(Token::Uint(above_u128)).is_err());
        assert!(u64::from_token(Token::Uint(Uint::from(u64::MAX) + 1)).is_err());
        assert!(i8::from_token(Token::Uint(Uint::MAX)).is_err());
        assert!(i8::from_token(Token::Int(Uint::from(i8::MAX.unsigned_abs()) + 1)).is_err());

        let below_i8_min = Uint::zero().overflowing_sub(Uint::from(i8::MIN.unsigned_abs()) + 1).0;
        assert!(i8::from_token(Token::Int(below_i8_min)).is_err());
    }

    #[test]
    fn should_decode_signed_abi_integer_boundaries() {
        assert_eq!(i8::from_token(i8::MIN.into_token()).unwrap(), i8::MIN);
        assert_eq!(i8::from_token(i8::MAX.into_token()).unwrap(), i8::MAX);
        assert_eq!(i128::from_token(i128::MIN.into_token()).unwrap(), i128::MIN);
        assert_eq!(i128::from_token(i128::MAX.into_token()).unwrap(), i128::MAX);
    }

    #[test]
    fn should_sign_extend_negative_integers() {
        assert_eq!((-1i8).into_token(), Token::Int(Uint::MAX));
        assert_eq!((-2i16).into_token(), Token::Int(Uint::MAX - 1));
        assert_eq!((-3i32).into_token(), Token::Int(Uint::MAX - 2));
        assert_eq!((-4i64).into_token(), Token::Int(Uint::MAX - 3));
        assert_eq!((-5i128).into_token(), Token::Int(Uint::MAX - 4));
    }
}
