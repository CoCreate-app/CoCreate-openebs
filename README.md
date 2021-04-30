# Openebs Mayastor
Chart is forked from [Original Source](https://github.com/openebs/Mayastor)

### Prerequisite
1) kubectl should be installed and configured in machine from which deployment will be made
2) helm 3 should be installed and configured
3) nodes should be assigned label ```nodeType=storage``` && ```openebs.io/engine=mayastor```
4) Add taints to the storage nodes ```openeebs=storage:NoSchedule```
5) Add tolerations to the taints ```openeebs=storage:NoSchedule```s
6) There should be seperate storage devices attached otherwise ```/dev/sdb``` will be used by default
7) execute the following commands in storage nodes
```bash
$ grep HugePages /proc/meminfo
AnonHugePages:         0 kB
ShmemHugePages:        0 kB
HugePages_Total:    1024
HugePages_Free:      671
HugePages_Rsvd:        0
HugePages_Surp:        0
$ echo 512 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
$ echo vm.nr_hugepages = 512 | sudo tee -a /etc/sysctl.conf
$ grep vm.nr_hugepages = 512 /etc/sysctl.conf # verify that vm.nr_hugepages = 512 prints 
$ reboot
$ #in admin node or from rancher interface add the following label
$ kubectl label node storage-node-name openebs.io/engine=mayastor
```

## Installation
#### Command
```bash
$ make install
$ # or
$ make install ns=openebs-ls release=openebs-deployment
```
#### Command args
1) **ns**=yournamespace
2) **chart**=desriredoperatorchart
3) **lpath**=localpath
4) **release**=name
5) **valuesFile**=filename

## Uninstallation
```bash
$ make uninstall
```
## make targets
1) **clean**: Cleans the local fs
2) **install**: install the chart
3) **uninstall**: uninstalls the chart
4) **rke-yaml**: generate the deployment yamls
5) **createns**: create namespace
6) **removens**: delete namespace


