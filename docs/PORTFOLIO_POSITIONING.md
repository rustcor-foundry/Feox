# Feox Portfolio Positioning

Feox is the kernel-and-runtime foundation product in the broader software
portfolio.

## Portfolio Role

Feox covers:

- bare-metal bootstrap
- capability-oriented kernel direction
- hardware-shaped runtime primitives
- low-level execution and ownership boundaries below the rest of the product family

## Shared Portfolio Doctrine

Feox should follow the shared portfolio principle:

Build operator-first systems software that makes infrastructure, access, and
network operations safer, clearer, and faster.

For Feox specifically, that means:

- safer systems foundations through explicit ownership and narrow low-level contracts
- clearer runtime behavior through hardware-shaped abstractions instead of opaque convenience layers
- faster future platform work through a strong bare-metal base the rest of the portfolio can learn from or build against

## What Feox Is Not

Feox is not:

- a side research repo with no product intent
- a generic hobby kernel
- a desktop application or dashboard product
- a shortcut around disciplined bootstrap and runtime design

It should remain focused on becoming a serious low-level systems product with
clear architectural intent, even while the implementation is still early.

## Relationship To The Rest Of The Portfolio

Within the current portfolio:

- `Citadel` is the identity operations and access governance product
- `NetRadR` is the network visibility and intelligence product
- `Zenith` is the operator terminal and execution product
- `Pylon` is the storage appliance and clustered storage operations product
- `RustMon` is the node-level monitoring and diagnostics product
- `Feox` is the kernel-and-runtime foundation product

That separation should stay clear. Feox sits beneath the rest conceptually,
but it should not be treated as less important just because it is earlier.

## Strategic Identity

Feox should feel like:

- the serious low-level systems lane in the portfolio
- the place where ownership, execution, and runtime boundaries are made explicit
- the long-horizon kernel foundation product, not just a speculative notebook
