LAZY = $(shell echo run)
NOW := $(shell echo run)
APPEND_S := a
APPEND_S += b
APPEND_R = a
APPEND_R += $(NOW)
COND ?= fromcond
COND ?= ignored
all:
	echo "$(LAZY) $(NOW) $(APPEND_S) $(APPEND_R) $(COND)"
