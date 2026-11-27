#!/usr/bin/ucode
// SPDX-License-Identifier: MPL-2.0
'use strict';

import { cursor } from 'uci';
import { stat, writefile } from 'fs';

const source = ARGV[0], stage = ARGV[1];
function fail(message) {
    warn('unravel: ' + message + '\n');
    exit(1);
}

function boolean(value, location) {
    if (value == null)
        return 1;
    if (type(value) != 'string')
        fail(location + ' must be a scalar boolean');
    switch (lc(trim(value))) {
    case '1': case 'true': case 'yes': case 'y': case 'on': case 'enabled':
        return 1;
    case '0': case 'false': case 'no': case 'n': case 'off': case 'disabled':
        return 0;
    default:
        fail(location + ' must be a boolean');
    }
}

function options(section, exclude) {
    const result = {};
    for (let key in sort(keys(section)))
        if (substr(key, 0, 1) != '.' && !(key in exclude))
            result[key] = section[key];
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
    instances[name] = { id: name, root: {}, singletons: {}, entries: [],
                        enabled: boolean(section.enabled, name + '.enabled'), syslog: 1 };
    if (section.dry_run != null)
        instances[name].root.dry_run = section.dry_run;
    instances[name].root.network = options(section, { enabled: true, dry_run: true });
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
    if (kind in instance.singletons)
        fail(instance.id + ': duplicate ' + kind + ' sections');
    instance.singletons[kind] = true;
    switch (kind) {
    case 'law':
        const law = section.type;
        const own = options(section, { instance: true, type: true });
        if (law == null) {
            if (length(keys(own)))
                fail(name + ': a law with settings must specify its type');
            break;
        }
        if (type(law) != 'string' || law == 'name' || !match(law, /^[a-zA-Z0-9_]+$/))
            fail(name + ': invalid law selector');
        instance.root.law = { name: law };
        instance.root.law[law] = own;
        break;
    case 'output':
        instance.syslog = boolean(section.use_syslog, name + '.use_syslog');
        instance.root.output = options(section, { instance: true, use_syslog: true });
        break;
    default:
        instance.root[kind] = options(section, { instance: true });
        if (kind == 'reflectors' && 'entries' in instance.root.reflectors)
            fail(name + ': reflector entries must use reflector sections');
    }
}

for (let name, instance in instances) {
    if (!instance.enabled)
        continue;
    if (length(instance.entries)) {
        instance.root.reflectors ??= {};
        instance.root.reflectors.entries = instance.entries;
    }
    const payload = sprintf('%J\n', instance.root);
    const path = stage + '/' + name + '.json';
    // Check the actual length after writefile closes its buffered handle.
    if (writefile(path, payload) != length(payload) || stat(path)?.size != length(payload))
        fail(name + ': cannot write staged JSON');
    printf('%s %d\n', name, instance.syslog);
}
