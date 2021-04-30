# author: zeeshan
# email: zeeshanelsyed@gmail.com
ns ?= mayastor
# helm repo add openebs https://openebs.github.io/charts
chart ?= openebs
release ?= openebs-maystor
lpath ?=./src/chart
valuesFile ?= values.yaml

export cmd = -



red=$(shell tput setaf 1)
green=$(shell tput setaf 2)
yellow=$(shell tput setaf 3)
blue=$(shell tput setaf 4)
purple=$(shell tput setaf 5) 
end=$(shell tput sgr0)
$(shell printf " $(red)Namespace$(end): $(ns)\n\
$(green)Chart$(end): $(chart)\n\
$(yellow)Create Namespace$(end): $(blue)$(cn)$(end)\n" 1>&2)


help:
	@printf "\n--------> $(green)Description$(end): $(yellow)deploy redis cluster with redis operator charts$(end) <--------\n"
	@printf " *)$(purple)$/{Parameters}$(end) : {Required} : $/{Description} : $/{Usage}\n\
	 1)$(purple)ns$(end): required : namespace value : make $/{target} ns=yournamespace\n\
	 2)$(purple)chart$(end): optional :Operator Chart : make $/{target} chart=desriredoperatorchart\n\
	 4)$(purple)lpath$(end): optional :local path for charts : make $/{target} lpath=localpath\n\
	 5)$(purple)release$(end): optional : helm release name : make $/{target} release=name\n"
	 5)$(purple)valuesFile$(end): optional : helm release name : make $/{target} valuesFile=filename\n"
	 @printf "\n ------------> Examples <------------\n\
	 1) $(green)make $/{target} ns=namespace lpath=./charts/ release=openebs$(end)\n\
	 2) $(green)make $/{target} ns=namespace chart=chartname$(end)\n"
	
createns:
	kubectl create ns $(ns)
removens:
  ifneq ("(NotFound)",$(findstring "`kubectl get ns $(ns)`",(NotFound)))
	kubectl delete ns $(ns)
  endif
clean:
	rm -r $(release).yaml 
install:
	helm install $(release) $(lpath) -f $(lpath)/values.yaml --namespace=$(ns) --debug | cat > $(release).yaml
rke-yaml:
	helm template --name-template=$(release) $(lpath) --namespace=$(ns) | cat > $(release).yaml 
uninstall:
	helm uninstall $(release) && make clean