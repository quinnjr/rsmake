LIST = a b c
reverse = $(2) $(1)
all:
	echo "[$(foreach v,$(LIST),<$(v)>)]"
	echo "[$(call reverse,left,right)]"
	echo "[$(or ,,third)] [$(and a,b)] [$(and a,,c)]"
	echo "[$(origin LIST)] [$(origin NOTSET)] [$(origin CC)]"
