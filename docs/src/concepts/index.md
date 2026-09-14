<!-- description: Understand what each part of Orion is and why it behaves as it does: the three primitives, the lifecycle, packages, plugins, models, and the glossary. -->
<!-- type: hub -->
<!-- last_verified: 2026-09-14 -->

# Concepts

These pages give you the mental model before, or instead of, doing. Each defines one thing, says what problem it solves, shows how it works with a minimal example, and states its boundaries. None of them is a procedure; the guides they link to are.

Start with [How Orion works](./how-orion-works.md): the three primitives, one request's journey through the engine, and the five places the runtime can be extended.

Then the primitives, one page each. A [channel](./channels.md) is where traffic arrives and what contract a caller must satisfy. A [workflow](./workflows.md) is the ordered pipeline of tasks a channel runs. A [connector](./connectors.md) is a named connection to an external system that workflows reference by name.

Two pages cover governance. [The entity lifecycle](./lifecycle.md) is the one-way path from draft to active to archived that every versioned entity follows. [Packages](./packages.md) are the unit a service ships as, from one instance to another.

Two pages cover the extension points. A [plugin](./plugins.md) adds a task function as a sandboxed WebAssembly component. A [model](./models.md) adds an ONNX graph, admitted by the node before it serves.

Three pages are maps. [Architectural characteristics](./architectural-characteristics.md) lists everything the runtime carries, by quality attribute. [Design notes](./design-notes.md) hold the reasoning behind rules that look arbitrary until you read the argument. The [glossary](./glossary.md) defines every term the book uses with a fixed meaning.
