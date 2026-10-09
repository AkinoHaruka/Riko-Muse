# twin-companion

English | [中文](README.zh.md)

The plugin entry is currently inert. It does not alter or reject agent turns.

## Responsibility and next integration

This module owns companion behavior in the Twin bundle. Its next integration point is the `agent/pre-step` event, where companion guidance can be added without replacing the default agent loop.
