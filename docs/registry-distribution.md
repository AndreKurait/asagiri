# Registry inspection

`GET /v1.47/distribution/{reference}/json` reads an image's current registry
descriptor and platform metadata without pulling layers or adding a local
image. This supports image update checks in clients such as Watchtower and
Arcane. URL-encode the reference, including repository slashes and registry
ports.

The response contains `Descriptor` (`mediaType`, `digest`, `size`) and
`Platforms`. An image index includes every platform entry, including
attestations. For a single manifest, Asagiri reads and verifies the config
blob to obtain its platform. If that blob is unavailable, `Platforms` is
`null`, matching Docker's inspection response.

Each inspection contacts the registry, including references to digests
previously pulled into Asagiri. A deleted manifest returns 404; rejected
credentials return 401. The request's `X-Registry-Auth` header and the
configured registry transport and CA settings apply to both manifest and
config downloads. Asagiri also rejects content that does not match a
requested digest.

The port retains the authorship of
[Kiri PR #309](https://github.com/libracoder/kiri/pull/309), which addresses
[Kiri issue #257](https://github.com/libracoder/kiri/issues/257). The live
fixture in `tests/distribution.py` runs the shared compatibility cases
against Docker and Asagiri; digest verification is an additional Asagiri
check. Registry unit tests also prime the pull cache before deleting a
manifest or revoking access.

The Docker 28.0.4 reference returns HTTP 500 with `no basic auth credentials`
when the registry starts requiring Basic authentication and the request has
no credentials. Asagiri reports that rejection as HTTP 401. The fixture checks
each engine's specific response and prints the observed status and diagnostic;
it does not treat arbitrary server errors as successful authentication checks.
