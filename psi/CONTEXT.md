# Psi

A standalone agent runtime connecting the user interface, model, and tools.
This glossary belongs to the Psi experiment, separate from paclang.

## Language

**Psi**:
The name of the standalone agent runtime (agentic harness) connecting the user interface, model, and tools. Written with an initial capital letter.

**Session**:
A distinct conversation between a user and an agent, containing its turns.
_Avoid_: Turn as a synonym for the entire conversation.

**Turn**:
One unit of agent work accepted for execution in response to a user request within a session,
including model requests and tool invocations until completion or cancellation.
_Avoid_: An individual model request as a synonym for a turn.

**Tool invocation**:
One execution of a selected tool with specific arguments within a turn.
_Avoid_: Tool as a synonym for an individual invocation.

**Turn cancellation**:
Stopping further work on a turn and terminating the active work it owns.
Cancellation does not roll back external actions already performed.
_Avoid_: Rollback as a synonym for cancellation.

**Unknown action outcome**:
A state in which an action may have occurred, but the retained information
is insufficient to determine its result or establish that no external changes occurred.
_Avoid_: Failure or non-execution as a synonym for an unknown outcome.

**Action admission**:
One-time authorization from the selected rule for a specific action in a specific turn.
Admission is necessary but does not replace the other conditions for executing the action.
_Avoid_: Success or unconditional execution authorization as a synonym for admission.
