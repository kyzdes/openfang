# Baseline evidence

Raw output from reproducing each defect against the unpatched build, kept so that
"the patch fixed it" is a comparison and not an assertion.

One file is deliberately absent: a copy of a live `config.toml` was committed here by
mistake and removed in a later commit. It contained a real dashboard `api_key`, which
was rotated. Do not commit config snapshots — redact them, or capture only the section
under test.
