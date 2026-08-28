//! EDS type-signature parsing and Postcard-compatible dynamic values.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntegerType {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TypeKind {
    Bool,
    Integer(IntegerType),
    F32,
    F64,
    String,
    List(Box<EdsType>),
    Map(Box<EdsType>, Box<EdsType>),
    Product(Vec<EdsType>),
    Sum(Vec<EdsType>),
}

/// The representation-relevant portion of an EDS type signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdsType {
    kind: TypeKind,
    fields: HashMap<String, usize>,
}

impl EdsType {
    fn new(kind: TypeKind) -> Self {
        Self {
            kind,
            fields: HashMap::new(),
        }
    }

    /// Parses an EDS type/refinement signature, retaining wire layout and field names.
    pub fn parse(input: &str) -> Result<Self> {
        let mut parser = Parser::new(input);
        let ty = parser.parse_type()?;
        parser.skip_whitespace();
        ensure!(
            parser.is_eof(),
            "unexpected type-signature input at byte {}: {:?}",
            parser.position,
            &input[parser.position..]
        );
        Ok(ty)
    }

    fn field_index(&self, field: &str) -> Result<usize> {
        self.fields
            .get(field)
            .copied()
            .with_context(|| format!("field '{field}' not found in EDS type"))
    }

    fn indexed(&self, index: usize) -> Result<&EdsType> {
        match &self.kind {
            TypeKind::Product(types) | TypeKind::Sum(types) => types
                .get(index)
                .with_context(|| format!("EDS type index {index} is out of bounds")),
            TypeKind::List(element) => Ok(element),
            _ => bail!("EDS type does not support indexing"),
        }
    }
}

/// A dynamically decoded EDS value.
#[derive(Clone, Debug, PartialEq)]
pub enum EdsValue {
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    I128(i128),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    F32(f32),
    F64(f64),
    String(String),
    Sequence(Vec<EdsValue>),
    Variant(u32, Box<EdsValue>),
}

impl EdsValue {
    pub fn as_f64(&self) -> Result<f64> {
        match self {
            Self::F32(value) => Ok(f64::from(*value)),
            Self::F64(value) => Ok(*value),
            _ => bail!("expected floating-point EDS value, got {self:?}"),
        }
    }

    pub fn as_bool(&self) -> Result<bool> {
        match self {
            Self::Bool(value) => Ok(*value),
            _ => bail!("expected boolean EDS value, got {self:?}"),
        }
    }

    pub fn as_seq(&self) -> Result<&[EdsValue]> {
        match self {
            Self::Sequence(values) => Ok(values),
            _ => bail!("expected sequence EDS value, got {self:?}"),
        }
    }

    fn indexed(&self, index: usize) -> Result<&EdsValue> {
        match self {
            Self::Sequence(values) => values
                .get(index)
                .with_context(|| format!("EDS value index {index} is out of bounds")),
            Self::Variant(tag, value) if *tag as usize == index => Ok(value),
            _ => bail!("EDS value does not support index {index}"),
        }
    }
}

/// An EDS value paired with the type information needed for named field access.
#[derive(Clone, Debug, PartialEq)]
pub struct EdsFrame {
    ty: EdsType,
    pub data: EdsValue,
}

impl EdsFrame {
    pub fn new(ty: EdsType, data: EdsValue) -> Self {
        Self { ty, data }
    }

    /// Decodes one complete Postcard-encoded value using an EDS type signature.
    pub fn decode(ty: EdsType, bytes: &[u8]) -> Result<Self> {
        let data = decode(&ty, bytes)?;
        Ok(Self { ty, data })
    }

    /// Encodes this value using its EDS type signature.
    pub fn encode(&self) -> Result<Vec<u8>> {
        encode(&self.ty, &self.data)
    }

    pub fn get_by_field(&self, field: &str) -> Result<Self> {
        let index = self.ty.field_index(field)?;
        Ok(Self {
            ty: self.ty.indexed(index)?.clone(),
            data: self.data.indexed(index)?.clone(),
        })
    }
}

pub(crate) fn decode(ty: &EdsType, bytes: &[u8]) -> Result<EdsValue> {
    let mut decoder = Decoder { bytes, position: 0 };
    let value = decoder.decode(ty)?;
    ensure!(
        decoder.position == bytes.len(),
        "{} trailing bytes after EDS value",
        bytes.len() - decoder.position
    );
    Ok(value)
}

pub(crate) fn encode(ty: &EdsType, value: &EdsValue) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    encode_into(ty, value, &mut bytes)?;
    Ok(bytes)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Decoder<'_> {
    fn decode(&mut self, ty: &EdsType) -> Result<EdsValue> {
        Ok(match &ty.kind {
            TypeKind::Bool => match self.byte()? {
                0 => EdsValue::Bool(false),
                1 => EdsValue::Bool(true),
                value => bail!("invalid Postcard boolean byte {value}"),
            },
            TypeKind::Integer(integer) => self.decode_integer(*integer)?,
            TypeKind::F32 => EdsValue::F32(f32::from_le_bytes(self.array()?)),
            TypeKind::F64 => EdsValue::F64(f64::from_le_bytes(self.array()?)),
            TypeKind::String => {
                let length = usize::try_from(self.varint_u128()?)
                    .context("EDS string length exceeds usize")?;
                EdsValue::String(std::str::from_utf8(self.take(length)?)?.to_owned())
            }
            TypeKind::List(element) => {
                let length = usize::try_from(self.varint_u128()?)
                    .context("EDS list length exceeds usize")?;
                let mut values = Vec::with_capacity(length);
                for index in 0..length {
                    values.push(
                        self.decode(element)
                            .with_context(|| format!("list element {index}"))?,
                    );
                }
                EdsValue::Sequence(values)
            }
            TypeKind::Map(key, value) => {
                let length =
                    usize::try_from(self.varint_u128()?).context("EDS map length exceeds usize")?;
                let mut values = Vec::with_capacity(length);
                for index in 0..length {
                    values.push(EdsValue::Sequence(vec![
                        self.decode(key)
                            .with_context(|| format!("map key {index}"))?,
                        self.decode(value)
                            .with_context(|| format!("map value {index}"))?,
                    ]));
                }
                EdsValue::Sequence(values)
            }
            TypeKind::Product(types) => {
                let mut values = Vec::with_capacity(types.len());
                for (index, element) in types.iter().enumerate() {
                    values.push(
                        self.decode(element)
                            .with_context(|| format!("product element {index}"))?,
                    );
                }
                EdsValue::Sequence(values)
            }
            TypeKind::Sum(types) => {
                let tag = u32::try_from(self.varint_u128()?).context("sum tag exceeds u32")?;
                let variant = types
                    .get(tag as usize)
                    .with_context(|| format!("sum tag {tag} is out of bounds"))?;
                EdsValue::Variant(tag, Box::new(self.decode(variant)?))
            }
        })
    }

    fn decode_integer(&mut self, ty: IntegerType) -> Result<EdsValue> {
        let raw = self.varint_u128()?;
        Ok(match ty {
            IntegerType::I8 => EdsValue::I8(unzigzag(raw) as i8),
            IntegerType::I16 => EdsValue::I16(unzigzag(raw) as i16),
            IntegerType::I32 => EdsValue::I32(unzigzag(raw) as i32),
            IntegerType::I64 => EdsValue::I64(unzigzag(raw) as i64),
            IntegerType::I128 => EdsValue::I128(unzigzag(raw)),
            IntegerType::U8 => EdsValue::U8(u8::try_from(raw)?),
            IntegerType::U16 => EdsValue::U16(u16::try_from(raw)?),
            IntegerType::U32 => EdsValue::U32(u32::try_from(raw)?),
            IntegerType::U64 => EdsValue::U64(u64::try_from(raw)?),
            IntegerType::U128 => EdsValue::U128(raw),
        })
    }

    fn varint_u128(&mut self) -> Result<u128> {
        let mut value = 0u128;
        let mut shift = 0;
        loop {
            let byte = self.byte()?;
            ensure!(shift < 128, "Postcard varint exceeds 128 bits");
            value |= u128::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.position)
            .context("unexpected end of EDS data")?;
        self.position += 1;
        Ok(byte)
    }

    fn take(&mut self, length: usize) -> Result<&[u8]> {
        let end = self
            .position
            .checked_add(length)
            .context("EDS data length overflow")?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .context("unexpected end of EDS data")?;
        self.position = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("slice length was checked"))
    }
}

fn encode_into(ty: &EdsType, value: &EdsValue, bytes: &mut Vec<u8>) -> Result<()> {
    match (&ty.kind, value) {
        (TypeKind::Bool, EdsValue::Bool(value)) => bytes.push(u8::from(*value)),
        (TypeKind::Integer(integer), value) => encode_integer(*integer, value, bytes)?,
        (TypeKind::F32, EdsValue::F32(value)) => bytes.extend(value.to_le_bytes()),
        (TypeKind::F64, EdsValue::F64(value)) => bytes.extend(value.to_le_bytes()),
        (TypeKind::String, EdsValue::String(value)) => {
            encode_varint(value.len() as u128, bytes);
            bytes.extend(value.as_bytes());
        }
        (TypeKind::List(element), EdsValue::Sequence(values)) => {
            encode_varint(values.len() as u128, bytes);
            for value in values {
                encode_into(element, value, bytes)?;
            }
        }
        (TypeKind::Map(key, value_type), EdsValue::Sequence(values)) => {
            encode_varint(values.len() as u128, bytes);
            for item in values {
                let pair = item.as_seq()?;
                ensure!(pair.len() == 2, "EDS map item must contain a key and value");
                encode_into(key, &pair[0], bytes)?;
                encode_into(value_type, &pair[1], bytes)?;
            }
        }
        (TypeKind::Product(types), EdsValue::Sequence(values)) => {
            ensure!(
                types.len() == values.len(),
                "EDS product expected {} values, got {}",
                types.len(),
                values.len()
            );
            for (ty, value) in types.iter().zip(values) {
                encode_into(ty, value, bytes)?;
            }
        }
        (TypeKind::Sum(types), EdsValue::Variant(tag, value)) => {
            let variant = types
                .get(*tag as usize)
                .with_context(|| format!("sum tag {tag} is out of bounds"))?;
            encode_varint(u128::from(*tag), bytes);
            encode_into(variant, value, bytes)?;
        }
        _ => bail!("EDS value {value:?} does not match type {ty:?}"),
    }
    Ok(())
}

fn encode_integer(ty: IntegerType, value: &EdsValue, bytes: &mut Vec<u8>) -> Result<()> {
    let value = match (ty, value) {
        (IntegerType::I8, EdsValue::I8(value)) => zigzag(i128::from(*value)),
        (IntegerType::I16, EdsValue::I16(value)) => zigzag(i128::from(*value)),
        (IntegerType::I32, EdsValue::I32(value)) => zigzag(i128::from(*value)),
        (IntegerType::I64, EdsValue::I64(value)) => zigzag(i128::from(*value)),
        (IntegerType::I128, EdsValue::I128(value)) => zigzag(*value),
        (IntegerType::U8, EdsValue::U8(value)) => u128::from(*value),
        (IntegerType::U16, EdsValue::U16(value)) => u128::from(*value),
        (IntegerType::U32, EdsValue::U32(value)) => u128::from(*value),
        (IntegerType::U64, EdsValue::U64(value)) => u128::from(*value),
        (IntegerType::U128, EdsValue::U128(value)) => *value,
        _ => bail!("EDS integer value {value:?} does not match {ty:?}"),
    };
    encode_varint(value, bytes);
    Ok(())
}

fn encode_varint(mut value: u128, bytes: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn zigzag(value: i128) -> u128 {
    ((value << 1) ^ (value >> 127)) as u128
}

fn unzigzag(value: u128) -> i128 {
    ((value >> 1) as i128) ^ -((value & 1) as i128)
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn parse_type(&mut self) -> Result<EdsType> {
        let mut variants = vec![self.parse_postfix()?];
        while self.consume('+') {
            variants.push(self.parse_postfix()?);
        }
        if variants.len() == 1 {
            Ok(variants.pop().unwrap())
        } else {
            Ok(EdsType::new(TypeKind::Sum(variants)))
        }
    }

    fn parse_postfix(&mut self) -> Result<EdsType> {
        let mut ty = self.parse_atom()?;
        while self.consume('?') {
            ty = EdsType::new(TypeKind::Sum(vec![
                EdsType::new(TypeKind::Product(Vec::new())),
                ty,
            ]));
        }
        Ok(ty)
    }

    fn parse_atom(&mut self) -> Result<EdsType> {
        self.skip_whitespace();
        if self.consume_str("#[") {
            return self.parse_tensor();
        }
        match self.peek() {
            Some('(') => self.parse_tuple(),
            Some('[') => self.parse_list(),
            Some('{') => self.parse_braces(),
            Some('!') => {
                self.position += 1;
                Ok(EdsType::new(TypeKind::Sum(Vec::new())))
            }
            Some(_) => self.parse_scalar(),
            None => bail!("unexpected end of EDS type signature"),
        }
    }

    fn parse_tuple(&mut self) -> Result<EdsType> {
        self.expect('(')?;
        if self.consume(')') {
            return Ok(EdsType::new(TypeKind::Product(Vec::new())));
        }
        let first_position = self.position;
        let first_name = self.try_field_name()?;
        if first_name.is_none() {
            self.position = first_position;
        }
        let first = self.parse_type()?;
        if self.consume(')') && first_name.is_none() {
            return Ok(first);
        }

        let mut types = vec![first];
        let mut names = vec![first_name];
        self.expect(',')?;
        while !self.consume(')') {
            let position = self.position;
            let name = self.try_field_name()?;
            if name.is_none() {
                self.position = position;
            }
            names.push(name);
            types.push(self.parse_type()?);
            if !self.consume(',') {
                self.expect(')')?;
                break;
            }
        }
        product_with_fields(types, names)
    }

    fn parse_list(&mut self) -> Result<EdsType> {
        self.expect('[')?;
        let position = self.position;
        if let Some(first_name) = self.try_field_name()? {
            let first_type = self.parse_type()?;
            let mut names = vec![first_name];
            let mut types = vec![first_type];
            while self.consume(',') && !self.consume(']') {
                names.push(
                    self.try_field_name()?
                        .context("expected named EDS list item")?,
                );
                types.push(self.parse_type()?);
            }
            if self.previous_char() != Some(']') {
                self.expect(']')?;
            }
            ensure!(
                types.windows(2).all(|pair| pair[0].kind == pair[1].kind),
                "named EDS list items must have the same wire type"
            );
            let mut ty = EdsType::new(TypeKind::List(Box::new(types.remove(0))));
            ty.fields = names.into_iter().enumerate().map(|(i, n)| (n, i)).collect();
            return Ok(ty);
        }
        self.position = position;

        let element = self.parse_type()?;
        let mut fields = HashMap::new();
        if self.consume(';') {
            self.expect('(')?;
            let mut index = 0;
            while !self.consume(')') {
                fields.insert(self.parse_name()?, index);
                index += 1;
                if !self.consume(',') {
                    self.expect(')')?;
                    break;
                }
            }
        }
        self.expect(']')?;
        let mut ty = EdsType::new(TypeKind::List(Box::new(element)));
        ty.fields = fields;
        Ok(ty)
    }

    fn parse_tensor(&mut self) -> Result<EdsType> {
        self.skip_whitespace();
        let position = self.position;
        if let Ok(length) = self.parse_usize()
            && self.consume(']')
        {
            return Ok(EdsType::new(TypeKind::Product(vec![
                EdsType::new(
                    TypeKind::F64
                );
                length
            ])));
        }
        self.position = position;
        let element = self.parse_type()?;
        self.expect(';')?;
        let length = self.parse_usize()?;
        self.expect(']')?;
        ensure!(
            length <= 1_000_000,
            "EDS tensor length is unreasonably large"
        );
        Ok(EdsType::new(TypeKind::Product(vec![element; length])))
    }

    fn parse_braces(&mut self) -> Result<EdsType> {
        self.expect('{')?;
        let first = self.parse_type()?;
        if self.consume(':') {
            let value = self.parse_type()?;
            self.expect('}')?;
            return Ok(EdsType::new(TypeKind::Map(
                Box::new(first),
                Box::new(value),
            )));
        }
        self.expect('|')?;
        self.skip_refinement()?;
        Ok(first)
    }

    fn parse_scalar(&mut self) -> Result<EdsType> {
        let name = self.parse_name()?;
        let kind = match name.as_str() {
            "bool" => TypeKind::Bool,
            "i8" => TypeKind::Integer(IntegerType::I8),
            "i16" => TypeKind::Integer(IntegerType::I16),
            "int" | "i32" => TypeKind::Integer(IntegerType::I32),
            "i64" => TypeKind::Integer(IntegerType::I64),
            "i128" => TypeKind::Integer(IntegerType::I128),
            "u8" => TypeKind::Integer(IntegerType::U8),
            "u16" => TypeKind::Integer(IntegerType::U16),
            "u32" => TypeKind::Integer(IntegerType::U32),
            "u64" => TypeKind::Integer(IntegerType::U64),
            "u128" | "id" | "fid" => TypeKind::Integer(IntegerType::U128),
            "str" => TypeKind::String,
            "f32" => TypeKind::F32,
            "float" | "f64" | "1" => TypeKind::F64,
            "body_eci" | "body_ecef" => TypeKind::Product(vec![EdsType::new(TypeKind::F64); 4]),
            "eci" | "ecef" | "lla" | "gc" | "body" | "llaDeg" | "gcLla" | "gcLlaDeg" => {
                TypeKind::Product(vec![EdsType::new(TypeKind::F64); 3])
            }
            _ => TypeKind::F64,
        };
        self.skip_unit_expression()?;
        Ok(EdsType::new(kind))
    }

    fn try_field_name(&mut self) -> Result<Option<String>> {
        self.skip_whitespace();
        let position = self.position;
        let Ok(name) = self.parse_name() else {
            self.position = position;
            return Ok(None);
        };
        if self.consume(':') {
            Ok(Some(name))
        } else {
            self.position = position;
            Ok(None)
        }
    }

    fn parse_name(&mut self) -> Result<String> {
        self.skip_whitespace();
        if matches!(self.peek(), Some('"' | '\'')) {
            let quote = self.peek().unwrap();
            self.position += quote.len_utf8();
            let start = self.position;
            while let Some(character) = self.peek() {
                if character == quote {
                    let name = self.input[start..self.position].to_owned();
                    self.position += quote.len_utf8();
                    return Ok(name);
                }
                self.position += character.len_utf8();
            }
            bail!("unterminated quoted EDS field name");
        }
        let start = self.position;
        while let Some(character) = self.peek() {
            if character.is_whitespace()
                || matches!(
                    character,
                    '(' | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | ':'
                        | ';'
                        | '+'
                        | '?'
                        | '|'
                        | '*'
                        | '/'
                        | '^'
                )
            {
                break;
            }
            self.position += character.len_utf8();
        }
        ensure!(self.position > start, "expected EDS type or field name");
        Ok(self.input[start..self.position].to_owned())
    }

    fn parse_usize(&mut self) -> Result<usize> {
        self.skip_whitespace();
        let start = self.position;
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.position += 1;
        }
        ensure!(self.position > start, "expected EDS array length");
        Ok(self.input[start..self.position].parse()?)
    }

    fn skip_unit_expression(&mut self) -> Result<()> {
        loop {
            self.skip_whitespace();
            if !matches!(self.peek(), Some('*' | '/' | '^')) {
                return Ok(());
            }
            self.position += 1;
            self.skip_whitespace();
            if self.previous_char() == Some('^') && self.peek() == Some('-') {
                self.position += 1;
            }
            let _ = self.parse_name()?;
        }
    }

    fn skip_refinement(&mut self) -> Result<()> {
        let mut braces = 0usize;
        let mut quote = None;
        while let Some(character) = self.peek() {
            self.position += character.len_utf8();
            if let Some(expected) = quote {
                if character == expected {
                    quote = None;
                }
                continue;
            }
            match character {
                '"' | '\'' => quote = Some(character),
                '{' => braces += 1,
                '}' if braces == 0 => return Ok(()),
                '}' => braces -= 1,
                _ => {}
            }
        }
        bail!("unterminated EDS refinement")
    }

    fn expect(&mut self, expected: char) -> Result<()> {
        ensure!(
            self.consume(expected),
            "expected '{expected}' at byte {}",
            self.position
        );
        Ok(())
    }

    fn consume(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.peek() == Some(expected) {
            self.position += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn consume_str(&mut self, expected: &str) -> bool {
        self.skip_whitespace();
        if self.input[self.position..].starts_with(expected) {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += self.peek().unwrap().len_utf8();
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn previous_char(&self) -> Option<char> {
        self.input[..self.position].chars().next_back()
    }

    fn is_eof(&self) -> bool {
        self.position == self.input.len()
    }
}

fn product_with_fields(types: Vec<EdsType>, names: Vec<Option<String>>) -> Result<EdsType> {
    let mut ty = EdsType::new(TypeKind::Product(types));
    for (index, name) in names.into_iter().enumerate() {
        if let Some(name) = name {
            ensure!(
                ty.fields.insert(name.clone(), index).is_none(),
                "duplicate EDS field name '{name}'"
            );
        }
    }
    Ok(ty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realistic_target_type_and_accesses_flat_fields() {
        let ty = EdsType::parse(
            r#"(time: f64, position: eci, "threat.relative_position": #[f64; 3], visible: bool)"#,
        )
        .unwrap();
        let frame = EdsFrame::new(
            ty,
            EdsValue::Sequence(vec![
                EdsValue::F64(60_000.0),
                EdsValue::Sequence(vec![
                    EdsValue::F64(1.0),
                    EdsValue::F64(2.0),
                    EdsValue::F64(3.0),
                ]),
                EdsValue::Sequence(vec![
                    EdsValue::F64(4.0),
                    EdsValue::F64(5.0),
                    EdsValue::F64(6.0),
                ]),
                EdsValue::Bool(true),
            ]),
        );

        assert_eq!(
            frame.get_by_field("time").unwrap().data.as_f64().unwrap(),
            60_000.0
        );
        assert_eq!(
            frame
                .get_by_field("threat.relative_position")
                .unwrap()
                .data
                .as_seq()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn postcard_wire_examples_decode_and_round_trip() {
        let cases = [
            ("bool", EdsValue::Bool(true), vec![1]),
            ("i32", EdsValue::I32(-64), vec![127]),
            ("u128", EdsValue::U128(300), vec![0xac, 0x02]),
            (
                "str",
                EdsValue::String("abc".into()),
                vec![3, b'a', b'b', b'c'],
            ),
            (
                "[u16]",
                EdsValue::Sequence(vec![EdsValue::U16(1), EdsValue::U16(300)]),
                vec![2, 1, 0xac, 2],
            ),
        ];
        for (signature, expected, bytes) in cases {
            let ty = EdsType::parse(signature).unwrap();
            assert_eq!(decode(&ty, &bytes).unwrap(), expected);
            assert_eq!(encode(&ty, &expected).unwrap(), bytes);
        }
    }

    #[test]
    fn parses_structural_signatures_and_ignores_refinements() {
        for signature in [
            "(x: float, y: #[m/s; 3], enabled: bool)",
            "{str: (f64, f64?)}",
            "[float; (x, y, z)]",
            "[x: m, y: m^2]",
            "{#[km/s; 3] | eci}",
            "() + str + (u64, fid)",
        ] {
            EdsType::parse(signature).unwrap_or_else(|error| panic!("{signature}: {error:#}"));
        }
    }
}
