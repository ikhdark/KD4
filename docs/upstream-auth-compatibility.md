# Auth/provider upstream restoration

This local restoration uses compatible upstream snapshots, not a wholesale update
to upstream `main`:

These crates now live in the repository-root `DO-NOT-CHANGE/` and follow its frozen
maintenance instructions. Cargo package names and public APIs are unchanged.

| Crate | Upstream source |
| --- | --- |
| `login` | `e1b7b1acb3ccf6ba9295762679414934c894b914` |
| `backend-client` | `506a328dab110591d3c1449a15217596e7e9cd61`; HTTP client and request tests from `888be42a20c5a727214d898c4e335ac2e27161af` |
| `model-provider-info` | shared fork/upstream base `2e8c3756f95789c215d9ea9a5ade6ec377934b3f` |

Latest upstream (`06f97622f8529feef0d230fc37519c36c6bb2eeb`) requires a
broader application-network-policy, workload-identity, gateway-OAuth, and Bedrock
integration migration. Those features are not enabled by this restoration.

Compatibility adaptations retained in the existing owners:

- Login uses the fork's fallible, asynchronous HTTP constructors and existing
  verified/unverified agent-identity JWT APIs. Custom-CA errors remain errors.
- Asynchronous browser-server binding and the prepare/persist device-login split
  remain available to app-server callers. Preparing device credentials does not
  write the auth store.
- Backend clients can replace their auth snapshot without replacing their shared
  transport pool. Analytics keeps the fork's local model definitions. Plan and
  rate-limit conversion uses the protocol types supported by this checkout.
- Provider endpoint/retry accessors and the Astra catalog identifier remain for
  existing consumers. Retry counts use the fork transport's `max_retries` API.
  The existing internal standalone-web-search field remains excluded from config;
  this does not add custom-provider standalone search support.
- Tests use this fork's `require_network!` prerequisite, rather than treating an
  unavailable network as a successful skipped test. The Pro display expectation
  follows the current shared protocol label.

The checkout uses Cargo, not the removed upstream Bazel workspace. No installed
binary is replaced and Desktop is not restarted by this source restoration.
