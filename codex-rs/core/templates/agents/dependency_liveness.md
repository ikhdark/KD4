# Dependency liveness

Before a worker waits, identify the dependency producer and the actual event/sender that releases it. Break cycles such as root waiting for a readiness report while that live worker waits for root's grant. Ancestor progress-message restrictions still apply: never make a forbidden readiness message the prerequisite for release. Use the existing queue or a prearranged peer handoff; otherwise schedule the bounded owner before it waits, or keep tightly coupled work local.

On producer completion or failure, resolve its named dependents using existing task state. Do not leave a waiter behind a finished owner, invent status-only follow-ups, or treat an elapsed timeout as permission to take the resource. Missing release ownership is a concrete blocker, not an indefinite preparation phase.
