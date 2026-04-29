# bitrouter-accounts

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Provides account/session persistence, JWT key revocation storage, and virtual
key storage for short `brv_...` credentials. Virtual key rows store the JWT and
a hash of the virtual key; the raw virtual key itself is shown once and is not
persisted.
