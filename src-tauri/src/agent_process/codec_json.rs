// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Strict JSON value decoding used by the JSONL codec.
//!
//! Keeping duplicate-key detection in its own module prevents framing code
//! from mixing byte-boundary rules with recursive JSON semantics.

use crate::agent_process::codec::CodecError;
use serde::de::Deserializer as _;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt::Formatter;

/// 使用递归 `DeserializeSeed` 检查每一层 object 的重复 key，避免嵌套参数污染。
pub(super) fn parse_strict_value(text: &str) -> Result<Value, CodecError> {
    let mut duplicate = false;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = deserializer
        .deserialize_any(StrictValueVisitor {
            duplicate: &mut duplicate,
        })
        .map_err(|_| {
            if duplicate {
                CodecError::DuplicateKey
            } else {
                CodecError::InvalidJson
            }
        })?;
    if duplicate {
        return Err(CodecError::DuplicateKey);
    }
    deserializer.end().map_err(|_| CodecError::InvalidJson)?;
    Ok(value)
}

struct StrictValueSeed<'a> {
    duplicate: &'a mut bool,
}

impl<'de> serde::de::DeserializeSeed<'de> for StrictValueSeed<'_> {
    type Value = Value;

    /// 递归套用同一 visitor，确保数组/对象内部也拒绝 duplicate key。
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor {
            duplicate: self.duplicate,
        })
    }
}

struct StrictValueVisitor<'a> {
    duplicate: &'a mut bool,
}

impl<'de> serde::de::Visitor<'de> for StrictValueVisitor<'_> {
    type Value = Value;

    /// 提供 serde 所需的稳定类型描述，避免错误路径泄露原始 JSON。
    fn expecting(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    /// 保留 JSON boolean，同时不引入可变共享状态。
    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    /// 把有符号整数映射到 serde_json number，维持协议数值语义。
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    /// 把无符号整数映射到 serde_json number，避免字符串化造成 schema 漂移。
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    /// 拒绝无法表示的浮点值，防止 NaN/Infinity 穿过 JSON 边界。
    fn visit_f64<E>(self, value: f64) -> Result<Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }

    /// 复制 borrowed string，使返回树独立于输入 buffer 生命周期。
    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    /// 直接保留 owned string，避免再次做无意义的编码转换。
    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    /// 将 serde 的 absent-like value 归一化为 JSON null。
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    /// 将 unit 归一化为 JSON null，保持 Value 表示闭包。
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    /// 递归展开 optional value，保证嵌套 object 继续经过严格 visitor。
    fn visit_some<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    /// 递归读取数组元素，防止 duplicate key 检测只覆盖顶层对象。
    fn visit_seq<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = access.next_element_seed(StrictValueSeed {
            duplicate: &mut *self.duplicate,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    /// 对每个 object 独立维护 seen 集合，发现重复字段立即终止解析。
    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut seen = HashSet::new();
        let mut object = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                *self.duplicate = true;
                return Err(serde::de::Error::custom("duplicate object key"));
            }
            let value = access.next_value_seed(StrictValueSeed {
                duplicate: &mut *self.duplicate,
            })?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}
