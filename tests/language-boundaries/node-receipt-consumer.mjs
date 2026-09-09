import fs from 'node:fs';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

const [schemaPath, instancePath, outputPath] = process.argv.slice(2);
if (!schemaPath || !instancePath || !outputPath) {
  throw new Error('usage: node-receipt-consumer.mjs <schema> <valid-instance> <output>');
}
const ajvModule = process.env.AJV_MODULE;
if (!ajvModule) {
  throw new Error('AJV_MODULE must name the exact Ajv 2020 module installed by CI');
}

const { default: Ajv2020 } = await import(pathToFileURL(ajvModule).href);
const schema = JSON.parse(fs.readFileSync(schemaPath, 'utf8'));
const ingress = JSON.parse(fs.readFileSync(instancePath, 'utf8'));
const contract = schema?.$defs?.GitCliInstallReceiptV1;
if (!contract) {
  throw new Error('authored schema does not expose $defs.GitCliInstallReceiptV1');
}

const ajv = new Ajv2020({ allErrors: true, strict: true });
const validate = ajv.compile(contract);
const requireValid = (label, value) => {
  if (!validate(value)) {
    throw new Error(`${label} failed authored JSON Schema validation: ${ajv.errorsText(validate.errors)}`);
  }
};
const requireInvalid = (label, value) => {
  if (validate(value)) {
    throw new Error(`${label} unexpectedly passed authored JSON Schema validation`);
  }
};

// Ingress proves a JavaScript/Node consumer accepts the same canonical wire
// shape as the Rust interface implementation.
requireValid('ingress receipt', ingress);

// Egress is deliberately reconstructed field-by-field rather than cloned. It
// proves the Node producer can emit the shared wire contract without relying on
// generated TypeSpec schema as an authority.
const egress = {
  schema_version: ingress.schema_version,
  source: ingress.source,
  revision: ingress.revision,
  binary: ingress.binary,
  installed_path: ingress.installed_path,
  sha256: ingress.sha256,
  manifest: ingress.manifest,
  flags_contract: ingress.flags_contract,
};
requireValid('egress receipt', egress);

// Fail closed on representative compatibility violations: required-field loss,
// unknown wire members, and a type mismatch.
const missingRequired = { ...egress };
delete missingRequired.binary;
requireInvalid('missing required binary', missingRequired);
requireInvalid('unknown wire member', { ...egress, unexpected: true });
requireInvalid('schema_version type mismatch', { ...egress, schema_version: '1' });

fs.writeFileSync(outputPath, `${JSON.stringify(egress, null, 2)}\n`);
console.log(JSON.stringify({
  status: 'passed',
  ingress: 'passed',
  egress: 'passed',
  rejectedNegativeCases: 3,
}));
