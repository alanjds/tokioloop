# Project Brief: RLoop

## Project Overview

TokioLoop is an AsyncIO selector event loop implemented in Rust on top of the tokio crate. This project aims to provide a high-performance alternative to Python's standard library event loop implementation. This project also hosts RLoop, based on mio crate.

## Core Requirements & Goals

### Primary Goals
- Implement a fully-compatible AsyncIO event loop in Rust
- Provide better performance than Python's standard implementation
- Maintain API compatibility with existing AsyncIO code
- Support Unix systems (Linux and macOS)

### Technical Requirements
- Use tokio crate for cross-platform I/O multiplexing
- Provide Python bindings via PyO3
- Support Python 3.12+ (including free threading support)
- Implement core AsyncIO features: TCP, UDP, subprocess handling, timers

### Current Status
- **Development Status**: Alpha (Work in Progress)
- **Production Ready**: No - not suited for production usage
- **Platform Support**: Unix systems only (Linux, macOS)
- **Version**: 0.2.0

### Known Limitations
- Different behavior for `call_later` with negative delays
- **TokioLoop TCP server implementation broken** - continuous "Invalid argument (os error 22)" errors
- **TokioLoop TCP client implementation incomplete** - no real async I/O operations
- TCP functionality currently only works with RLoop (mio-based implementation)
- TokioLoop passes import tests but fails functional TCP tests

## Success Criteria
- Performance improvements over stdlib event loop
- Full compatibility with existing AsyncIO applications
- Stable API for production use
- Comprehensive test coverage

## Scope Definition
This project focuses on core AsyncIO functionality. Out of scope for initial releases:
- Windows support
- Advanced debugging features
- Performance profiling tools
- Alternative I/O backends beyond tokio
