# enhance-pir

`enhance-pir` provides client and protocol types for privately retrieving
Ironwood note enhancement records. It includes the optional `enhance-pir-cli`
binary behind the `cli` feature.

Version 0.0.1 speaks schema 11 of `ironwood-enhance-pir-v6`. Each 653-byte
record contains the encrypted ciphertext suffix and indexer metadata for one
Ironwood action. The wallet retains the ciphertext prefix and ephemeral key
from compact scanning, checks the server's chain anchor and coverage, and
authenticates the reconstructed note. The server's fee, expiry, and transparent
flags are indexer assertions rather than authenticated note fields.

The client uses the `simplepir-p16-q48-v1` profile. It can query rows privately,
but network timing and the number of row queries remain visible. Wallets must
use the matching protocol revision and independently validate returned records
against their local chain context.

This crate does not include the Enhance PIR server. The `cli` feature provides
a diagnostic command line client; it does not perform wallet acceptance or
note authentication.
