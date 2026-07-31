// Stripped at publish time by the `**/*.test.*` default exclude.
const assert = require('node:assert');
const { greet } = require('./index.js');

assert.strictEqual(greet('zed'), 'hello, zed');
