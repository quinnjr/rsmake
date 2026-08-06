LIST = a b c
reverse = $(2) $(1)
thunk = $(0):$(1)
flavor_defined = simple
all:
	echo "[$(foreach v,$(LIST),<$(v)>)]"
	echo "[$(call reverse,left,right)]"
	echo "[$(call thunk,val)]"
	echo "[$(or ,,third)] [$(and a,b)] [$(and a,,c)]"
	echo "[$(origin LIST)] [$(origin NOTSET)] [$(origin CC)]"
	echo "[$(flavor flavor_defined)] [$(flavor recipe)] [$(flavor never)]"
