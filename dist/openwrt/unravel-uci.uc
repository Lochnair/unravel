#!/usr/bin/ucode
// SPDX-License-Identifier: MPL-2.0
'use strict';

// Converts /etc/config/unravel into one JSON configuration per enabled
// instance. UCI values are strings; `unravel --config-schema` says which are
// booleans, numbers or lists. The schema only guides the conversion: a value
// it can't convert is passed on as it is, and `unravel --check-config`
// reports it.

import { cursor } from 'uci';
import { popen, stat, writefile } from 'fs';

const source = ARGV[0], stage = ARGV[1], program = ARGV[2] ?? '/usr/sbin/unravel';
function fail(message) {
    warn('unravel: ' + message + '\n');
    exit(1);
}

const BOOLEANS = {
    '1': true, 'true': true, 'yes': true, 'y': true, 'on': true, 'enabled': true,
    '0': false, 'false': false, 'no': false, 'n': false, 'off': false, 'disabled': false,
};

// The options the converter reads itself, which never reach unravel.
function boolean(value, location) {
    if (value == null)
        return true;
    const parsed = type(value) == 'string' ? BOOLEANS[lc(trim(value))] : null;
    if (parsed == null)
        fail(location + ' must be a boolean');
    return parsed;
}

function options(section, exclude) {
    const result = {};
    for (let key in sort(keys(section)))
        if (substr(key, 0, 1) != '.' && !(key in exclude))
            result[key] = section[key];
    return result;
}

function quote(word) {
    return "'" + replace(word, "'", "'\\''") + "'";
}

// The configuration schema, with a script's options when it has one.
const schemas = {};
function schema(script) {
    const key = script ?? '';
    if (key in schemas)
        return schemas[key];
    let command = '/usr/bin/env -i ' + quote(program) + ' --config-schema';
    if (script != null)
        command += ' --script ' + quote(script);
    const process = popen(command, 'r');
    if (!process)
        fail('cannot run ' + program);
    const output = process.read('all');
    if (process.close() != 0)
        fail(script == null ? 'cannot read the configuration schema'
                            : 'cannot read the options of ' + script);
    return schemas[key] = json(output);
}

// The one type a schema allows besides null, if there is one.
function single_type(schema) {
    const types = filter(type(schema?.type) == 'array' ? schema.type : [schema?.type],
                         t => t != null && t != 'null');
    return length(types) == 1 ? types[0] : null;
}

// For schemas with alternatives, the first whose constants all match.
function variant(schema, raw) {
    for (let branch in schema?.oneOf ?? [schema]) {
        let matches = true;
        for (let name, property in branch?.properties ?? {})
            if (property?.const != null && raw[name] != property.const)
                matches = false;
        if (matches)
            return branch;
    }
    return null;
}

const INTEGER = /^-?[0-9]+$/;
const NUMBER = /^-?([0-9]+\.?[0-9]*|\.[0-9]+)([eE][-+]?[0-9]+)?$/;

function value(schema, raw, location) {
    const text = type(raw) == 'string' ? trim(raw) : null;
    switch (single_type(schema)) {
    case 'boolean':
        const parsed = text != null ? BOOLEANS[lc(text)] : null;
        return parsed != null ? parsed : raw;
    case 'integer':
        return text != null && match(text, INTEGER) ? int(text) : raw;
    case 'number':
        return text != null && match(text, NUMBER) ? +text : raw;
    case 'array':
        if (single_type(schema.items) == 'object')
            fail(location + ' is a list of tables, which UCI cannot hold; use a JSON or TOML configuration');
        return map(type(raw) == 'array' ? raw : [raw], item => value(schema.items, item, location));
    default:
        return raw;
    }
}

// The paths an option name can stand for in an object schema. A table's
// fields are named after it: `download_delay_ms` can be `download.delay_ms`.
function paths(schema, name) {
    const found = [];
    for (let property, sub in schema?.properties ?? {}) {
        if (property == name)
            push(found, [property]);
        else if (single_type(sub) == 'object' && index(name, property + '_') == 0)
            for (let rest in paths(sub, substr(name, length(property) + 1)))
                push(found, [property, ...rest]);
    }
    return found;
}

// A section's options as the object `schema` describes. Options it doesn't
// know are passed on, for unravel to reject.
function object(schema, raw, location) {
    const result = {};
    for (let name in sort(keys(raw))) {
        const found = paths(schema, name);
        if (length(found) > 1)
            fail(location + '.' + name + ' is ambiguous: ' + join(' or ', map(found, p => join('.', p))));
        if (!length(found)) {
            result[name] = raw[name];
            continue;
        }
        const path = found[0];
        let node = result, sub = schema;
        for (let i = 0; i < length(path) - 1; i++) {
            sub = sub.properties[path[i]];
            if (type(node[path[i]]) != 'object')
                node[path[i]] = {};
            node = node[path[i]];
        }
        const last = path[length(path) - 1];
        node[last] = value(sub.properties[last], raw[name], location + '.' + join('.', path));
    }
    return result;
}

if (!source || !match(source, /^\/.*\/unravel$/) || !stage)
    fail('expected an absolute UCI path ending in /unravel and a staging directory');

const uci = cursor();
const sections = [];
// Loading the committed file by absolute path disables overlays and deltas.
if (!uci.load(source))
    fail('cannot load the committed UCI package: ' + uci.error());
uci.foreach('unravel', null, section => push(sections, section));

const instances = {};
for (let section in sections) {
    const name = section['.name'];
    if (section['.type'] != 'instance')
        continue;
    if (section['.anonymous'] || !match(name, /^[a-zA-Z0-9_]+$/))
        fail('instance sections require explicit names with letters, digits and underscores');
    instances[name] = { id: name, sections: {}, entries: [],
                        enabled: boolean(section.enabled, name + '.enabled'), syslog: true };
    instances[name].sections.network = options(section, { enabled: true, dry_run: true });
    if (section.dry_run != null)
        instances[name].dry_run = section.dry_run;
}

for (let section in sections) {
    const kind = section['.type'];
    const name = section['.name'];
    if (kind == 'instance')
        continue;
    if (!(kind in { law: true, reflectors: true, reflector: true, output: true, observability: true }))
        fail(name + ': unrecognized section type');
    if (type(section.instance) != 'string' || !(section.instance in instances))
        fail(name + ': option instance must identify a named instance');
    const instance = instances[section.instance];
    if (kind == 'reflector') {
        push(instance.entries, options(section, { instance: true }));
        continue;
    }
    if (kind in instance.sections)
        fail(instance.id + ': duplicate ' + kind + ' sections');
    switch (kind) {
    case 'law':
        const law = section.type;
        const own = options(section, { instance: true, type: true });
        instance.sections.law = own;
        if (law == null) {
            if (length(keys(own)))
                fail(name + ': a law with settings must specify its type');
            break;
        }
        if (type(law) != 'string' || law == 'name' || !match(law, /^[a-zA-Z0-9_]+$/))
            fail(name + ': invalid law selector');
        instance.law = law;
        break;
    case 'output':
        instance.syslog = boolean(section.use_syslog, name + '.use_syslog');
        instance.sections.output = options(section, { instance: true, use_syslog: true });
        break;
    default:
        instance.sections[kind] = options(section, { instance: true });
        if (kind == 'reflectors' && 'entries' in instance.sections.reflectors)
            fail(name + ': reflector entries must use reflector sections');
    }
}

for (let name, instance in instances) {
    if (!instance.enabled)
        continue;
    const script = instance.law == 'script' ? instance.sections.law.path : null;
    if (script != null && type(script) != 'string')
        fail(name + ': the law path must be a single option');
    const root = schema(script);
    const properties = root.properties;
    const config = {};
    if (instance.dry_run != null)
        config.dry_run = value(properties.dry_run, instance.dry_run, name + '.dry_run');
    for (let kind, raw in instance.sections) {
        if (kind == 'law') {
            if (instance.law == null)
                continue;
            config.law = { name: instance.law };
            const laws = properties.law?.properties ?? {};
            config.law[instance.law] = object(laws[instance.law], raw, name + '.law.' + instance.law);
        } else {
            config[kind] = object(properties[kind], raw, name + '.' + kind);
        }
    }
    if (length(instance.entries)) {
        const entry = properties.reflectors?.properties?.entries?.items;
        config.reflectors ??= {};
        config.reflectors.entries = map(instance.entries,
            raw => object(variant(entry, raw), raw, name + '.reflectors.entries'));
    }

    const payload = sprintf('%J\n', config);
    const path = stage + '/' + name + '.json';
    // Check the actual length after writefile closes its buffered handle.
    if (writefile(path, payload) != length(payload) || stat(path)?.size != length(payload))
        fail(name + ': cannot write staged JSON');
    printf('%s %d\n', name, instance.syslog ? 1 : 0);
}
