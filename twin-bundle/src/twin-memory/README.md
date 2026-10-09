# twin-memory

English | [中文](README.zh.md)

The plugin entry is currently inert. It reads no Session events, registers no tools, and makes no network requests.

## Responsibility and next integration

This module owns memory behavior in the Twin bundle. Its next integration point is Riko-Memory's `memoryd` HTTP API, including `POST /v1/context/compose`; transport, authentication, and error handling are not implemented here.
