# ADR-0001 — Rust for server and shared client core

Status: *accepted*

Server and all clients (Linux, Android, Windows, iOS) link the same Rustcrates; Flutter UIs bind via flutter_rust_bridge. Wire protocol and mediapipeline are implemented once, compiled per target.

*Rejected*: Go (no practical embedding in Flutter clients; protocol logicwould exist twice). C++ (memory-safety and build-system cost on a soloproject).

One ADR now establishes the habit — every significant decision from Phase 1 on gets one.
