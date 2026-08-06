A = 1
B =
all:
ifeq ($(A),1)
	echo a-is-one
else
	echo a-not-one
endif
ifdef B
	echo b-defined
else ifdef A
	echo a-defined
else
	echo neither
endif
ifneq "$(A)" "2"
	echo a-not-two
endif
