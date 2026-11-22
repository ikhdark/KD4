"""Serialization compatibility for validated generated protocol defaults."""

from enum import Enum

from pydantic import BaseModel, model_serializer


class WireDefaultModel(BaseModel):
    @model_serializer(mode="wrap")
    def _serialize_protocol_defaults(self, handler):
        # Keep omitted defaults' original serialized shape, including the
        # notification budget, without exposing untyped defaults to callers.
        original = None
        fields = type(self).model_fields
        for name, field in fields.items():
            if not field.validate_default or name in self.model_fields_set:
                continue
            value = getattr(self, name)
            if isinstance(value, BaseModel):
                emitted = value.model_dump(mode="json", by_alias=True, exclude_unset=True)
            elif isinstance(value, Enum):
                emitted = value.value
            elif isinstance(value, list) and all(isinstance(item, Enum) for item in value):
                emitted = [item.value for item in value]
            else:
                continue
            if emitted != field.default:
                continue
            if original is None:
                original = self.model_copy()
            object.__setattr__(original, name, field.default)
        return handler(self if original is None else original)
