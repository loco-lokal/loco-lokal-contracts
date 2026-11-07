//! Domain logic: authorization (`lib/ability.rb`), certificates
//! (`lib/generate_certificate.rb`), and account helpers. Ports of `lib/submissions/`,
//! `lib/submitters/`, and `lib/templates/` follow in Phases 3–5.

pub mod ability;
pub mod certificates;
pub mod timezones;
