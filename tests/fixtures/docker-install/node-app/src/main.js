const { containerGreeting } = require("@zed-pkg/docker-node-lib");

const greeting = containerGreeting();
if (greeting !== "hello from @zed-pkg/docker-node-lib") {
  throw new Error(`unexpected greeting: ${greeting}`);
}
console.log(greeting);
