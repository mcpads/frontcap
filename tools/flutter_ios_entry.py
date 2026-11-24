"""Live two-phase cold entry within the worker's existing Simulator and system-runner lease.

The Rust caller admits the installed scheme before this object retires the app. Records emitted
here are evidence; there is deliberately no constructor that resumes a saved pending entry.
"""
from native_host_process import write_json


class ColdEntry:
    def __init__(self, app, system, ready, number, directory, deadline, url):
        if system is None or ready.get('system_ui') != system.identity:
            raise ValueError('Cold iOS entry requires the independently owned system runner')
        system.ensure_running()
        request = app.entry_request(url, ready['owner']['generation'])
        if request['target'] != ready.get('external_url_target') or app.owner is None:
            raise ValueError('Cold iOS entry differs from current installed owner')
        self.app, self.system, self.ready = app, system, ready
        self.number, self.previous, self.finished = number, app.owner, False
        app.terminate(directory/'termination', deadline)
        accepted = app.begin_entry(directory/'entry', deadline, url)
        if accepted['request'] != request:
            raise ValueError('Cold iOS entry acceptance differs from admitted request')
        self.started = dict(command_number=number, previous=self.previous, entry=accepted)
        write_json(directory/'command.log', self.started)

    def poll(self, directory, deadline, generation, begin_command):
        if (self.finished or type(begin_command) is not int or begin_command != self.number
                or generation != self.ready['owner']['generation']):
            raise ValueError('Cold iOS readiness request differs from the original dispatch')
        self.system.ensure_running()
        observed = self.app.poll_entry(directory/'readiness', deadline)
        if observed['state'] == 'waiting':
            return dict(state='waiting', begin_command=self.number,
                        readiness={k:v for k,v in observed.items() if k != 'state'})
        if observed['state'] != 'ready' or observed['entry'] != self.started['entry']:
            raise ValueError('Cold iOS readiness differs from original acceptance')
        current = observed['application']
        endpoint = current['endpoint']
        ready = dict(self.ready, owner=dict(self.ready['owner'], generation=endpoint['generation']),
                     endpoint=dict(port=endpoint['port'], token=endpoint['token']))
        self.finished = True
        return dict(state='ready', begin_command=self.number,
                    replacement=dict(previous=self.previous, current=current, ready=ready,
                                     entry=self.started['entry']))
