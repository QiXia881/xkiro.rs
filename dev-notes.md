# Dev Notes

## MachineId normalization

- Kiro-Go and KAM both generate new account machine IDs as lowercase UUID v4 strings and persist them per account/credential.
- xkiro.rs keeps compatibility with existing stored values: UUID/32hex become canonical UUID, old xkiro bug forms `000...<uuid32>` and `<uuid32><uuid32>` migrate to UUID, other non-empty legacy values are preserved lowercase.
- New credentials must get a credential-level machineId during load/add/import/login persistence. `config.machineId` is legacy fallback only and must not be copied into every new credential.
- Do not reintroduce refreshToken/apiKey-derived machineId or zero-padded UUID generation.
