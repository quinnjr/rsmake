L = one two three four
all:
	echo "[$(word 2,$(L))] [$(words $(L))] [$(wordlist 2,3,$(L))]"
	echo "[$(firstword $(L))] [$(lastword $(L))]"
	echo "[$(word 9,$(L))] [$(wordlist 3,99,$(L))]"
	echo "[$(wordlist 2,2,$(L))] [$(wordlist 1,1,$(L))]"
	echo "[$(wordlist 3,2,$(L))] [$(wordlist 99,99,$(L))]"
