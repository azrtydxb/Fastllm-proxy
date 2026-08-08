# Test fixtures

`rsa_test_key.pem` is a throwaway 2048-bit RSA key, generated for this
repository's tests and used nowhere else. It exists because signing a Google
service-account assertion needs a real key — `ring` will not sign with a
synthetic one, and a test that skips the signature would not exercise the part
most likely to break.

It authorises nothing. It is not a credential for any account, and no Google
project has ever seen its public half.
