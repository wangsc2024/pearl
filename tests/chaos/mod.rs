//! Chaos tests -- 系統開發需求書 §61.
//!
//! These tests verify the system's resilience under adverse conditions:
//! - DB busy (simulate sqlite locked)
//! - Disk full (write to read-only path)
//! - Worker death mid-operation
