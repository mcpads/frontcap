"""Verify a request-bound remote shell gate and both sides of transport retirement."""
import hashlib
from native_android_guest_input import gate_command, parse_process, same_process


def check_setup(root,setup,record):
    record('guest_before_input.setup','shell_v2' in setup['features'].splitlines()
           and setup['adbd_pid']>0 and setup['binary_sha256']==setup['guest_binary_sha256']
           ==hashlib.sha256((root.parent/'input-gate').read_bytes()).hexdigest(),setup)


def check_guest(root,directory,row,setup,record,prefix):
    guest=row['guest'];ready=guest['ready'];identity=guest['identity'];request=row['transport_before']['request']
    killed=row['action']=='kill'
    record(prefix+'.guest_binding',ready['request']==request and ready['pid']==identity['pid']==identity['group']==ready['group']
           and ready['ppid']==identity['ppid']==setup['adbd_pid'] and ready['uid']==identity['uid']==2000
           and ready['hup_default'] is True and ready['hup_blocked'] is False
           and identity['exe']==guest['executable']==setup['remote']+'/input-gate'
           and guest['directory']==setup['remote']+'/'+row['action']
           and guest['released_before'] is None
           and (directory/'controller/input-transport.stdout').read_text()==f"GATE_READY {ready['pid']}\n",ready)
    values=[identity,guest['before_signal'],guest['before_release']]+[
        value['process'] for value in guest['stopped']+guest.get('after_host_death',[]) if value['process'] is not None]
    record(prefix+'.guest_identity',all(same_process(value,identity) and value['state']=='T'
           and value==parse_process(value['raw']) and not (value['sigign']|value['sigblk'])&1 for value in values)
           and not (identity['sigpnd']|identity['shdpnd']|guest['before_signal']['sigpnd']|guest['before_signal']['shdpnd'])&1,values)
    record(prefix+'.guest_command',row['transport_before']['command']==gate_command(
           row['transport_before']['command'][:3],guest['executable'],guest['directory'],request)
           and row['transport_before']['command'][1:3]==['-s','emulator-'+(root.parent/'emulator-port').read_text().strip()]
           and identity['command']==row['transport_before']['command'][-1][5:],row['transport_before']['command'])
    if killed:
        observed=guest['after_host_death'][-1]
        record(prefix+'.guest_disconnect',row['intervention']=='kill-host'
               and row['signal_monotonic_ns']<row['controller_finished_ns']<=observed['host_ns']<guest['release_ns']
               and (observed['process']['sigpnd']|observed['process']['shdpnd'])&1
               and (guest['before_release']['sigpnd']|guest['before_release']['shdpnd'])&1
               and guest['released_after_host_death'] is None and guest['released_after'] is None,observed)
    else:
        released=guest['released_after']
        record(prefix+'.guest_release',row['intervention']=='continue-guest'
               and row['signal_monotonic_ns']<=guest['release_ns']<row['transport_after']['finished_monotonic_ns']
               and released['pid']==ready['pid'] and released['monotonic_ns']>ready['monotonic_ns']
               and not (guest['before_release']['sigpnd']|guest['before_release']['shdpnd'])&1,released)
    record(prefix+'.guest_retired',guest['retired'][-1]['process'] is None
           and guest['retired'][-1]['host_ns']>guest['release_ns']
           and guest['cleanup_retired'][-1]['process'] is None and guest['cleanup_signals']==[],guest['retired'])
