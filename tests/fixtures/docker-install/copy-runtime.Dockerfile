# node:22.23.1-bookworm-slim index digest; matches the contract builder runtime.
FROM node:22.23.1-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3

WORKDIR /app
COPY --chown=node:node . .

RUN test -z "$(find .vendor/.zed node_modules/@zed-pkg -type l -print -quit)" \
    && test -x .vendor/.zed/zed-pkg/docker-node-lib/bin/docker-node-tool \
    && test -x .vendor/.zed/.bin/docker-node-tool \
    && test -f .vendor/.zed/zed-pkg/docker-node-lib/generated/output.txt \
    && test -f node_modules/@zed-pkg/docker-node-lib/generated/output.txt

ENV HOME=/home/node
USER node

CMD ["sh", "-euc", "test \"$(id -u)\" != 0; node src/main.js; .vendor/.zed/.bin/docker-node-tool; test -r .vendor/.zed/zed-pkg/docker-node-lib/generated/output.txt"]
