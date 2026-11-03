-- Project-owned export adapter. Canonical manuscript links stay unchanged.
-- Relative links that leave docs/ become links to the exact source commit;
-- links to other manuscripts in docs/ become links to their source files.
local root=assert(os.getenv('DISKPROXY_REPO_ROOT'),'Expected source repository root')
local commit=pandoc.system.with_working_directory(root,function()
 return pandoc.pipe('git',{'rev-parse','HEAD'},''):gsub('%s+$','')
end)
assert(commit:match('^[0-9a-f]+$') and #commit==40,'Expected exact source commit')
local source='https://github.com/firstpair/diskproxy/'
local function blob(path)
 assert(path~='' and not path:find('..',1,true),'Unexpected relative source link')
 local anchor=''
 local hash=path:find('#',1,true)
 if hash then anchor=path:sub(hash); path=path:sub(1,hash-1) end
 return source..'blob/'..commit..'/'..path..anchor
end
function Link(link)
 local t=link.target
 if t:sub(1,3)=='../' then
  link.target=blob(t:sub(4))
 elseif not t:find(':',1,true) and t:sub(1,1)~='#' and t:match('%.md') then
  link.target=blob('docs/'..t)
 end
 return link
end
