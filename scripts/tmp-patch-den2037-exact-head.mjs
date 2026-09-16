import { readFile, writeFile } from "node:fs/promises";

const path = ".github/workflows/den-2037-local-lock-consumer.yml";
let source = await readFile(path, "utf8");

const checkout = `        with:
          fetch-depth: 1
          persist-credentials: false
          show-progress: false`;
const exactCheckout = `        with:
          ref: \${{ github.event.pull_request.head.sha || github.sha }}
          fetch-depth: 1
          persist-credentials: false
          show-progress: false`;

const first = source.indexOf(checkout);
if (first < 0) throw new Error("missing first zed-cli checkout");
source = source.slice(0, first) + exactCheckout + source.slice(first + checkout.length);
const second = source.indexOf(checkout, first + exactCheckout.length);
if (second < 0) throw new Error("missing second zed-cli checkout");
source = source.slice(0, second) + exactCheckout + source.slice(second + checkout.length);

const nativeAnchor = `          show-progress: false

      - name: Install Rust quality components`;
const nativeAssert = `          show-progress: false

      - name: Assert exact candidate checkout
        env:
          INTENDED_SHA: \${{ github.event.pull_request.head.sha || github.sha }}
        run: |
          set -euo pipefail
          actual=$(git rev-parse HEAD)
          printf 'intended=%s\\nactual=%s\\n' "$INTENDED_SHA" "$actual"
          test "$actual" = "$INTENDED_SHA"

      - name: Install Rust quality components`;
if (!source.includes(nativeAnchor)) throw new Error("missing native assertion anchor");
source = source.replace(nativeAnchor, nativeAssert);

const sharedAnchor = `          show-progress: false

      - name: Check out exact shared lock-library snapshot`;
const sharedAssert = `          show-progress: false

      - name: Assert exact zed-cli evidence checkout
        env:
          INTENDED_SHA: \${{ github.event.pull_request.head.sha || github.sha }}
        run: |
          set -euo pipefail
          actual=$(git rev-parse HEAD)
          printf 'intended=%s\\nactual=%s\\n' "$INTENDED_SHA" "$actual"
          test "$actual" = "$INTENDED_SHA"

      - name: Check out exact shared lock-library snapshot`;
if (!source.includes(sharedAnchor)) throw new Error("missing shared assertion anchor");
source = source.replace(sharedAnchor, sharedAssert);

await writeFile(path, source);
